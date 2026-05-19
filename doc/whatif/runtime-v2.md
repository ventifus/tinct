# What If: Runtime v2 — AST Redesign, Native Value Types, and Async Parallel Evaluation

**State:** Draft — 2026-05-19

**Supersedes:**
- [`ast-value-types.md`](ast-value-types.md) — fully absorbed
- [`async-eval.md`](async-eval.md) — fully absorbed

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

1. **AST Redesign** — split `Expr` into `SurfaceExpr` (immutable, `Send + Sync`, no `RefCell`, exposed to tinct) and `CoreExpr` (de Bruijn indices as plain fields, evaluator-internal only). Move compile-time-only declaration forms out of the expression enum. Assign stable `NodeId` to every node; move typechecker annotations into side tables.

2. **Native AST Value Types** — `Value::AstFile(Arc<SurfaceFile>)`, `Value::AstDoc(Arc<SurfaceDoc>)`, `Value::AstExpr(Arc<Spanned<SurfaceExpr>>)`. Declare `AstExpr`, `AstDoc`, `AstFile` as nominal types in prelude; tinct code pattern-matches on `AstExpr` variants with static typing. `load` returns `AstFile`. `expand` takes and returns `AstFile`. `eval` takes `[Seq AstExpr]`. `dict_to_file` is never written. `dict_to_ast` is deleted.

3. **Async Runtime** — `eval` and `materialize` become `async fn`. `Rc<T>` → `Arc<T>` throughout; `RefCell<T>` → `RwLock<T>` or `Mutex<T>`; `ThunkState` replaced by an `OnceLock`-based pair. Multi-thread Tokio with work-stealing distributes independent thunks across all cores. `task`/`await`/`channel`/`select` primitives. These are one refactor: every file is touched for the `async fn` contagion anyway; the `Rc`→`Arc` migration is mechanical on top.

These three parts are inseparable. The `Rc`→`Arc` migration (Part 3) requires all `Value`s to be `Send`; `Value::AstExpr` wrapping a type with `RefCell` fields is not `Send` — so AST redesign (Part 1) is a prerequisite. The native value types (Part 2) must use `Arc` to be compatible with the parallel runtime; doing them with `Rc` first and migrating later opens the same files twice. The thunk's internal `UnevaluatedState::Expr` stores `CoreExpr` (Part 1), not raw `Expr`. All three parts form one coherent implementation sprint.

---

## Part 1: AST Redesign

### The Two Representations

The current `Expr` enum serves every phase of the pipeline — parser, macro expander, resolver, typechecker, evaluator — with RefCell fields mutated in place by different passes. The redesign splits this into two clean types with a lowering pass between them.

**`SurfaceExpr`** — what the parser produces and what tinct code sees:
- Immutable; no `RefCell` fields
- `Arc`-wrapped at every recursive position
- Every node has a stable `NodeId` assigned at parse time
- Represents the user-visible structure: names, annotations, source forms
- Maps 1:1 to the tinct `AstExpr` type declaration
- Wrapped in `Value::AstExpr` — the representation tinct metaprogramming operates on

**`CoreExpr`** — what the evaluator operates on:
- De Bruijn coordinates as plain `u32` fields (no `RefCell`, no `Option`)
- Produced by the lowering pass from `SurfaceExpr`
- Stored inside `UnevaluatedState::Expr` in each thunk
- Never exposed to tinct code
- Can be optimized freely (e.g., unboxed closures, direct field access) without affecting the tinct API

### Node Identity

Every `SurfaceExpr` node carries `id: NodeId` — a `u32` assigned sequentially at parse time and stable across the node's lifetime. `NodeId` enables side tables indexed by node identity without pointer aliasing:

```rust
pub struct NodeId(u32);

// In the resolver:
pub struct ResolutionTable(HashMap<NodeId, (u32, u32)>);   // level, slot

// In the typechecker:
pub struct TypeAnnotationTable(HashMap<NodeId, Type>);      // resolved_type for TypeAssert
```

`ResolutionTable` replaces `VarRef.resolved: RefCell<...>`. `TypeAnnotationTable` replaces `TypeAssert.resolved_type: RefCell<...>`. Both are produced by their respective passes and threaded through the pipeline as explicit data rather than being mutated into the AST.

### `SurfaceExpr`

```rust
pub enum SurfaceExpr {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // Variable reference — escaped: true = $name (pin in patterns), false = bare (bind)
    // No 'resolved' field — resolution lives in ResolutionTable
    VarRef { name: String, escaped: bool },

    // Access
    DotAccess { expr: Arc<Spanned<SurfaceExpr>>, field: DotKey },
    Pipe       { lhs: Arc<Spanned<SurfaceExpr>>, rhs: Arc<Spanned<SurfaceExpr>> },

    // Sequential let* scoping (multi-expr fn bodies, match arm bodies)
    Sequential(Vec<Arc<Spanned<SurfaceExpr>>>),

    // Dict/list literal — entry key is None for auto-indexed (positional) entries
    Dict(Vec<Spanned<SurfaceEntry>>),

    // Function call — implied: true = [f x y], false = [call f x y]
    Call {
        func: Arc<Spanned<SurfaceExpr>>,
        args: Vec<Arc<Spanned<SurfaceExpr>>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        implied: bool,
    },

    // Function definition — desugared: true = synthesised by $_ desugaring
    // return_ann and resolved_type both live in SurfaceExpr, not side tables,
    // because return_ann is surface-level user input (not a pass result)
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<SurfaceParam>>,
        body: Arc<Spanned<SurfaceExpr>>,
        desugared: bool,
    },

    // Type assertion — no resolved_type field; lives in TypeAnnotationTable
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<SurfaceExpr>>,
    },

    // Annotated bare word, e.g. Fn@Number
    Annotated { name: String, annotation: Spanned<Annotation> },

    // Row variable / open record marker — None = unnamed (...)
    Rest(Option<String>),

    // Pattern matching
    Match {
        scrutinee: Arc<Spanned<SurfaceExpr>>,
        arms: Vec<SurfaceMatchArm>,
    },

    // Quasiquoting
    Quote(Arc<Spanned<SurfaceExpr>>),
    Unquote(Arc<Spanned<SurfaceExpr>>),
    UnquoteSplice(Arc<Spanned<SurfaceExpr>>),

    // Binding and pattern forms (used in fn params, match arms, instance arms)
    PatternDecl { bindings: Vec<Spanned<SurfaceExpr>> },
    LetDecl     { bindings: Vec<Spanned<SurfaceExpr>> },
    CaseArm     { pattern: Arc<Spanned<SurfaceExpr>>, body: Arc<Spanned<SurfaceExpr>> },
    TypeApp     { func: Arc<Spanned<SurfaceExpr>>, arg: Arc<Spanned<SurfaceExpr>> },

    // Placeholder `...` — evaluates to error when forced
    Placeholder,

    // Parse error node — span covers the unparseable region
    Error(Span),
}
```

Every `Spanned<SurfaceExpr>` also carries `id: NodeId`. In practice `Spanned` is extended:

```rust
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
    pub id: NodeId,  // NEW — stable across clone, assigned at parse time
}
```

### Declaration Separation

Compile-time-only forms — `TypeAlias`, `ClassDecl`, `InstanceDecl`, `DefMacro`, `MacroDecl`, `SyntaxClass`, `Splice` — are removed from `SurfaceExpr` and placed in a `SurfaceDecl` enum. They cannot produce runtime values and their presence in the expression enum forces every evaluator match arm to handle them (typically with `unreachable!()`).

```rust
pub enum SurfaceDecl {
    TypeAlias  { params: Vec<String>, body: Arc<Spanned<SurfaceExpr>> },
    ClassDecl  { name: String, params: Vec<String>, superclasses: Vec<(String, String)>,
                 methods: Vec<Spanned<SurfaceEntry>>, determines: Vec<Spanned<SurfaceExpr>>,
                 resolver: Option<Arc<Spanned<SurfaceExpr>>>, resolver_injective: bool },
    InstanceDecl { class_name: String, arms: Vec<(Spanned<SurfaceExpr>, Vec<Spanned<SurfaceEntry>>)> },
    DefMacro   { name: String, params: Arc<Spanned<SurfaceExpr>>, body: Arc<Spanned<SurfaceExpr>> },
    MacroDecl  { name: String, params: Arc<Spanned<SurfaceExpr>>, body: Arc<Spanned<SurfaceExpr>> },
    SyntaxClass { name: String, pattern: Arc<Spanned<SurfaceExpr>>, message: Option<String> },
    Splice     (Vec<Spanned<SurfaceExpr>>),
}
```

`SurfaceDoc` expresses this split via `SurfaceItem`:

```rust
pub enum SurfaceItem {
    Expr(Spanned<SurfaceExpr>),
    Decl(Spanned<SurfaceDecl>),
}

pub struct SurfaceDoc {
    pub id: NodeId,
    pub stage: Option<Stage>,
    pub name: Option<String>,
    pub items: Vec<SurfaceItem>,
    pub output_type: Option<Spanned<Annotation>>,
    pub expects: Option<Spanned<Annotation>>,
    pub caps: Option<Spanned<Vec<(String, Annotation)>>>,
}

pub struct SurfaceFile {
    pub documents: Vec<Spanned<SurfaceDoc>>,
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
    Pipe       { lhs: Arc<Spanned<CoreExpr>>,  rhs: Arc<Spanned<CoreExpr>> },
    Sequential(Vec<Arc<Spanned<CoreExpr>>>),
    Dict(Vec<Spanned<CoreEntry>>),
    Call { func: Arc<Spanned<CoreExpr>>, args: Vec<Arc<Spanned<CoreExpr>>>,
           named_args: Vec<Spanned<CoreNamedArg>>, implied: bool },
    Fn   { return_ann: Option<Spanned<Annotation>>, params: Vec<Spanned<CoreParam>>,
           body: Arc<Spanned<CoreExpr>>, desugared: bool },
    TypeAssert { annotation: Spanned<Annotation>, expr: Arc<Spanned<CoreExpr>>,
                 resolved_type: Option<Type> },  // set during lowering from TypeAnnotationTable
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

The lowering pass `lower(surface: &Spanned<SurfaceExpr>, res: &ResolutionTable, types: &TypeAnnotationTable) -> Spanned<CoreExpr>` is called once per expression when the `eval` builtin receives `[Seq AstExpr]`. The result is stored in the thunk's `UnevaluatedState::Expr`. The lowering pass is fast (one tree walk) and its output is cached in the thunk — subsequent materializations find the thunk already resolved.

`CoreExpr` is never exposed to tinct code. It lives only in thunks.

---

## Part 2: Native AST Value Types

### New Value Variants

```rust
Value::AstFile(Arc<SurfaceFile>)
Value::AstDoc(Arc<SurfaceDoc>)
Value::AstExpr(Arc<Spanned<SurfaceExpr>>)
```

`Arc` throughout — these variants are `Send + Sync`, compatible with the async parallel runtime from Part 3. No `Value::AstSpan` — spans remain plain Dicts (infrequently accessed, allocation cost is fine).

### Tinct Type Declarations

Added to prelude — the primitive signatures of `load`, `expand`, `eval`, `eval-types`, and `ast-of` reference these types, and they must be resolvable everywhere without an explicit import.

#### Supporting Types

```tinct
AstSpan: [type [AstSpan
  file:       String
  start-line: Int
  start-col:  Int
  end-line:   Int
  end-col:    Int]]

AstDotKey: [type [Ident String] [Index Int]]

AstParam: [type [AstParam
  name:       String
  annotation: AstAnnotation
  variadic:   Bool
  span:       AstSpan]]

AstNamedArg: [type [AstNamedArg
  name:  String
  value: AstExpr
  span:  AstSpan]]

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
  [Simple       value: String]
  [PropertyDict entries: [Seq AstAnnotationEntry]]
  [Annotated    name: String  inner: AstAnnotation]]

AstAnnotationEntry: [type [AstAnnotationEntry
  key:   String
  value: AstAnnotation]]
```

`AstPattern` maps the `Pattern` enum (`Wildcard`, `Variable`, `Literal`, `TypeTag`, `Pin`, `Dict`, `Seq`, `Constructor`, `Or`); its full declaration is deferred to the implementation sprint — the same approach applies.

#### `AstExpr`

Variants map 1:1 to `SurfaceExpr` members. Internal implementation fields are not exposed. Formatter-relevant flags (`escaped`, `implied`, `desugared`) are exposed.

```tinct
AstExpr: [type
  # Literals
  [IntLiteral   value: Int    span: AstSpan]
  [FloatLiteral value: Float  span: AstSpan]
  [BoolLiteral  value: Bool   span: AstSpan]
  [StrLiteral   value: String span: AstSpan]

  # Variables — escaped: true = $name (pin in patterns), false = bare name (bind)
  [Var  name: String  escaped: Bool  span: AstSpan]

  # Access
  [DotAccess  target: AstExpr  field: AstDotKey  span: AstSpan]

  # Pipe operator (desugar-only)
  [Pipe  lhs: AstExpr  rhs: AstExpr  span: AstSpan]

  # Sequential let* scoping (multi-expr fn bodies, match arm bodies)
  [Sequential  exprs: [Seq AstExpr]  span: AstSpan]

  # Dict/list literal — key: [] for auto-indexed entries
  [Dict  entries: [Seq AstEntry]  span: AstSpan]

  # Function call — implied: true = [f x y], false = [call f x y]
  [Call  fn: AstExpr  args: [Seq AstExpr]  named: [Seq AstNamedArg]  implied: Bool  span: AstSpan]

  # Function definition — desugared: true = synthesised by $_ desugaring
  [Fn  params: [Seq AstParam]  body: AstExpr  return-ann: AstAnnotation  desugared: Bool  span: AstSpan]

  # Type assertion — no resolved-type field (typechecker internal, lives in CoreExpr)
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

  # Higher-kinded type application in annotation positions
  [TypeApp  fn: AstExpr  arg: AstExpr  span: AstSpan]

  # Binding and pattern forms (used inside fn params, match arms, instance arms)
  [PatternDecl  bindings: [Seq AstExpr]          span: AstSpan]
  [LetDecl      bindings: [Seq AstExpr]          span: AstSpan]
  [CaseArm      pattern: AstExpr  body: AstExpr  span: AstSpan]

  # Placeholder `...` — evaluates to error when forced
  [Placeholder  span: AstSpan]

  # Parse error node — span covers the unparseable region
  [Error  span: AstSpan]]
```

Compile-time-only declaration forms (`TypeAlias`, `ClassDecl`, `InstanceDecl`, `DefMacro`, `MacroDecl`, `SyntaxClass`, `Splice`) are NOT in `AstExpr`. They appear in `SurfaceDoc.items` as `SurfaceItem::Decl` nodes. Tinct code iterating `doc.expressions` sees only value-producing expressions; type-system and macro declarations are accessible via a separate `doc.declarations` field.

#### `AstDoc` and `AstFile`

```tinct
DocName: [type [Named String] [Unnamed]]

AstDoc: [type [AstDoc
  stage:        [type [Runtime] [Type]]
  name:         DocName
  expressions:  [Seq AstExpr]    # value-producing expressions only
  declarations: [Seq AstDecl]    # compile-time-only forms
  output-type:  AstAnnotation    # [] when absent
  expects:      AstAnnotation]]  # [] when absent

AstFile: [type [AstFile
  documents: [Seq AstDoc]]]

AstDecl: [type
  [TypeAlias   params: [Seq String]  body: AstExpr]
  [ClassDecl   name: String  params: [Seq String]  ...]
  [InstanceDecl class-name: String   arms: [Seq AstExpr]]
  [DefMacro    name: String  params: AstExpr  body: AstExpr]
  [MacroDecl   name: String  params: AstExpr  body: AstExpr]
  [SyntaxClass name: String  pattern: AstExpr  message: String]
  [Splice      forms: [Seq AstExpr]]]
```

### Match Dispatch for `Value::AstExpr`

`Value::AstExpr` participates in `match` using the same protocol as `Value::Variant`. When the `match` builtin encounters `Value::AstExpr(e)`, it calls `surface_expr_match_view(e) -> (&'static str, Value::Dict)` to extract the variant tag and a shallow payload dict of the immediate fields. Match arm binding proceeds exactly as for `Value::Variant { tag, payload }`.

The payload dict is materialized per arm — one small allocation containing only the immediate fields of the matched node. Recursive children remain `Value::AstExpr` until accessed.

`Value::AstDoc` and `Value::AstFile` follow the same protocol via `surface_doc_match_view` and `surface_file_match_view`.

### Field Access on `Value::AstExpr`

Direct field access (`node.name`, `[get "span" node]`) is supported via the same field dispatcher used to build the match payload. The type checker warns that field access on a union type may not be valid for all variants — accurate, and better than the `Any` return type the Dict schema provided. Code requiring typed access should use `match`.

### Changed Primitive Signatures

```tinct
# After include-decomp                              # After runtime-v2
load@[Fn [source@String  name: @String] Dict]       load@[Fn [source@String  name: @String] AstFile]
expand@[Fn [ast@Dict] Dict]                         expand@[Fn [ast@AstFile] AstFile]
eval@[Fn [exprs@Dict  %: @Any  env: @Dict] Any]     eval@[Fn [exprs@[Seq AstExpr]  %: @Any  env: @Dict] Any]
eval-types@[Fn [exprs@Dict] Any]                    eval-types@[Fn [exprs@[Seq AstExpr]] Any]
ast-of@[Fn [expr@Any] Dict]                         ast-of@[Fn [expr@Any] AstExpr]
```

### Updated Include-Decomp Tinct Code

Structure unchanged; type annotations and the `doc.name` field access update:

```tinct
eval-document-runtime: [fn@[return: Dict] [let state doc@AstDoc include-dir]
  [result: [eval
    doc.expressions          # [Seq AstExpr] — passed directly to eval builtin
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
```

`doc.name` becomes `DocName` (`[Named String] | Unnamed`) rather than `String | []` — making the optional-name pattern explicit and type-safe.

### Quasiquoting and `eval-ast`

`[quote expr]` returns `Value::AstExpr` (was `Value::Dict`). `eval-ast` is deleted — replaced by `[eval [seq some-ast-expr] %: [] env: []]`.

### `ast_to_dict_expr` / `dict_to_ast` Fate

- **`dict_to_ast`** — deleted entirely; no longer called anywhere
- **`dict_to_file`** — never written; this proposal supersedes that item
- **`ast_to_dict_expr`** — retained for `deep-materialize` and JSON output only
- **`deep-materialize` on `Value::AstExpr`** — produces `Value::Variant` tree (`[Var name: "x" span: {...}]`), not string-keyed Dict. JSON output: `{"Var": {"name": "x", "span": {...}}}`. Breaking change to external AST JSON consumers — acceptable with no released users.

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
| `Rc<SurfaceFile/Doc/Expr>` | `Arc<SurfaceFile/Doc/Expr>` | New variants — Arc from day one |

`Arc` clone costs ~10–50ns vs ~1ns for `Rc`. Thunk evaluation costs microseconds to milliseconds. The overhead is negligible.

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
    Expr    { expr: Spanned<CoreExpr>, env: Arc<Environment>, ctx: Arc<EvalContext> },
    Builtin { func: BuiltinFn, args: Vec<Arc<Thunk>>, named: IndexMap<String, Arc<Thunk>>,
              depth: usize, call_span: Span, ctx: Arc<EvalContext> },
    Call    { func: Arc<Thunk>, args: Vec<Arc<Thunk>>, call_span: Span, ctx: Arc<EvalContext> },
}
```

Note `Spanned<CoreExpr>` — not `Spanned<Expr>` or `Spanned<SurfaceExpr>`. The lowering pass runs when the `eval` builtin receives `[Seq AstExpr]`; it converts each `SurfaceExpr` to `CoreExpr` and wraps the result in a thunk's `UnevaluatedState::Expr`. Thunks created directly by the evaluator during evaluation of `CoreExpr` also store `CoreExpr`.

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

**Cycle detection:** each async task maintains a task-local `HashSet<*const Thunk>` of thunks on its own evaluation stack. Demanding a thunk in this set is a cycle (same task). Seeing `None` in `unevaluated` while the result isn't set means another task is evaluating — wait via `OnceCell`. `EvalError` is now `Arc<EvalError>` (was `Box`) for cheap cross-thread clone.

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

### Serve and Connect Layers

Transport primitives produce connections; protocol layers transform them. Two generic factories in `stdlib/serve.llt` cover all server-side protocol layers:

```tinct
# 1:1 — each incoming connection is transformed into one outgoing connection
make-serve-layer: [fn [let accept-fn]
  [fn [let conn-ch config]
    [out: [channel 100]]
    [task [loop [fn [let]
      [match [recv conn-ch]
        [case [let raw: Ok]  [send out [accept-fn raw config]]]
        [case [let _: Err]   null]]]]]
    out]]

# 1:N — each incoming connection produces multiple items
make-multiplex-serve: [fn [let conn-fn]
  [fn [let conn-ch]
    [out: [channel 1000]]
    [task [loop [fn [let]
      [match [recv conn-ch]
        [case [let conn: Ok]  [task [conn-fn conn out]]]
        [case [let _: Err]    null]]]]]
    out]]
```

Concrete serve layers are instances:

```tinct
# Connection-promotion (1:1)
tls-serve:   [make-serve-layer tls-accept]      # Handle   → TlsHandle
h2-serve:    [make-serve-layer h2-accept]       # Handle   → H2Conn
h3-serve:    [make-serve-layer h3-accept]       # QuicConn → H3Conn
ws-serve:    [make-serve-layer ws-accept]       # Handle   → WsConn

# Message-extraction (1:N)
http1-serve:     [make-multiplex-serve http1-conn]   # Handle → RawRequest*
http2-requests:  [make-multiplex-serve http2-req-conn]
http3-requests:  [make-multiplex-serve http3-req-conn]
```

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
                      AstExpr/AstDoc/AstFile/AstParam/AstEntry/... type declarations
  strings.llt       — trim, pad-left/right, starts-with?, ends-with?, str-contains?, str-replace,
                      str-split-lines, words, unwords
  seq.llt           — zip-with, enumerate, chunk, partition, group-by, sort-by, uniq-by,
                      flat-map, scan, window, interleave
  path.llt          — path-join, path-dirname, path-basename, path-ext, path-normalize
  result.llt        — and-then, map-ok, map-err, unwrap-or, unwrap, ok?, err?, collect-results
  cap.llt           — narrow, readable?, writable?, with-temp

  net.llt           — Port type, parse-url, url-encode/decode, form-encode/decode, resolve-host
  http1.llt         — HTTP/1.1 framing in pure tinct on top of Handle
  http3.llt         — thin wrapper around h3-request Rust builtin
  serve.llt         — make-serve-layer, make-multiplex-serve; concrete serve/connect layers
  http.llt          — http-channel (unified TCP+QUIC); fetch; router, middleware

  async.llt         — exit, graceful-exit, finally, loop-select, retry
  datetime.llt      — Timestamp, Duration, formatting/parsing
  regex.llt         — Thompson NFA regex engine
  toml.llt          — complete TOML 1.0 parser
  sql.llt           — lazy SQL data sources
```

The `AstExpr` and supporting type declarations live in `prelude.llt` (not a separate module) because `eval`, `load`, `expand` reference them in primitive signatures.

---

## What Would Change

### Add to `src/ast.rs`

- `NodeId(u32)` — stable node identity assigned at parse time
- `SurfaceExpr` enum — immutable, `Arc`-recursive, no `RefCell`
- `SurfaceDecl` enum — compile-time-only declaration forms
- `SurfaceItem` enum — `Expr | Decl` for doc items
- `SurfaceDoc`, `SurfaceFile` structs — clean, no `RefCell`, `caps:` retained
- `CoreExpr` enum — de Bruijn as plain fields, evaluator-only
- `ResolutionTable(HashMap<NodeId, (u32, u32)>)` — replaces `VarRef.resolved: RefCell<...>`
- `TypeAnnotationTable(HashMap<NodeId, Type>)` — replaces `TypeAssert.resolved_type: RefCell<...>`
- Lowering pass `lower(surface, res, types) -> CoreExpr` in `src/lower.rs`

### Delete from `src/ast.rs`

- `Expr` enum — replaced by `SurfaceExpr` + `CoreExpr`
- `Document` struct — replaced by `SurfaceDoc`
- `File` struct — replaced by `SurfaceFile`

### Add to `src/value.rs`

- `Value::AstFile(Arc<SurfaceFile>)`
- `Value::AstDoc(Arc<SurfaceDoc>)`
- `Value::AstExpr(Arc<Spanned<SurfaceExpr>>)`
- `Value::Task(Arc<Mutex<TaskState>>)`
- `Value::Channel(Arc<ChannelInner>)`
- `Value::Context(tokio_util::sync::CancellationToken)`

### Change `src/value.rs`

- All `Rc<T>` → `Arc<T>` throughout; all `RefCell<T>` → `RwLock<T>` or `Mutex<T>`
- `ThunkState` enum replaced by `(Mutex<Option<UnevaluatedState>>, tokio::sync::OnceCell<...>)` pair

### Change `src/ast_dict.rs`

- **Add** `surface_expr_match_view(expr: &Spanned<SurfaceExpr>) -> (&'static str, Value)` — variant tag + shallow payload Dict; called by match evaluator
- **Add** `surface_expr_get_field(expr: &Spanned<SurfaceExpr>, field: &str, ctx: &Arc<EvalContext>) -> Option<Value>` — called by dot-access and `get`
- **Add** analogues for `SurfaceDoc` and `SurfaceFile`
- **Delete** `dict_to_ast` — removed entirely
- **Retain** `ast_to_dict_expr` — used only by `deep-materialize` and JSON output (now operates on `SurfaceExpr`)

### Change `src/eval.rs`

- All functions become `async fn`
- `eval` pattern-matches on `CoreExpr` (not `Expr`); all arms updated
- `eval_dict` fans out independent entries via `tokio::task::JoinSet` — automatic parallel evaluation
- **Add** `Value::AstExpr`, `Value::AstDoc`, `Value::AstFile` arms to match evaluator, dot-access evaluator, `get`, `has?`, `dict?`, `type-of`, `deep-materialize`, JSON serializer
- `materialize` uses the `OnceLock` forcing protocol
- `EvalContext` gains `cancel: CancellationToken` field

### Change `src/builtins_meta.rs`

- `load`: parse → `SurfaceFile` → wrap in `Value::AstFile`; `ast_to_dict` call deleted
- `expand`: unwrap `AstFile` → `&SurfaceFile`; call `expand()`; wrap result. No round-trip.
- `eval`: iterate `[Seq AstExpr]`; unwrap each `Value::AstExpr` → `Arc<Spanned<SurfaceExpr>>`; lower to `CoreExpr`; wrap in thunk; return
- `eval-types`: same as `eval` but uses `ctx.config.type_stage_env`
- `ast-of`: return `Value::AstExpr` wrapping argument thunk's `SurfaceExpr` (was `ast_to_dict_expr`)
- **Delete** `builtin_eval_ast` — replaced by `eval` on a single-element seq

### Change `src/expand.rs`

- Operates on `SurfaceExpr`/`SurfaceFile` instead of `Expr`/`File`
- `ast_expr_match_view` and `dict_to_ast` replace the existing Dict round-trip
- The shadow guard was already deleted in `include-decomp-primitives`

### Change `src/resolve.rs`

- Resolver produces `ResolutionTable` instead of mutating `VarRef.resolved` in place
- Returns `(SurfaceFile, ResolutionTable)` — the surface AST is unchanged by resolution

### Change `src/typecheck.rs`

- `TypeAnnotationTable` produced alongside type inference; no mutation of `TypeAssert.resolved_type`
- `AstExpr`, `AstDoc`, `AstFile` are declared types in prelude; resolved normally via type declaration
- **Add** `Type::Task(Box<Type>)`, `Type::Channel(Box<Type>)`, `Type::Context` — inference rules for async primitives

### Change `src/builtins.rs`

- `BuiltinFn` becomes `async fn` pointer type returning `Pin<Box<dyn Future>>`
- All ~180 builtins gain async wrapper
- I/O builtins replace `block_on(fut)` with `fut.await`
- **Add** new async primitive builtins (see table below)

### New Builtins

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `context` | `→ Context` | Current evaluation's cancellation context |
| `with-cancel` | `Context → [Context Fn]` | Child context + cancel function |
| `with-timeout` | `Context → Duration → Context` | Auto-cancels after duration |
| `with-deadline` | `Context → Timestamp → Context` | Auto-cancels at absolute time |
| `cancelled?` | `Context → Bool` | True if context cancelled |
| `with-context` | `Context → Fn → T` | Evaluates fn under given context |
| `timeout` | `Duration → Task@T → Result@T` | Awaits task with deadline |
| `cancel-root` | `→ Null` | Cancel root token — signals all tasks to stop |
| `drain` | `→ Null` | Await until all in-flight tasks finish |
| `exit-now` | `Int → Null` | `process::exit` immediately |
| `task` | `expr → Task@T` | Spawn evaluation of expr |
| `await` | `Task@T → T` | Suspend until task completes |
| `await-all` | `[Seq Task@T] → [Seq T]` | Await all; results in submission order |
| `await-any` | `[Seq Task@T] → T` | Return first completed; abort rest |
| `channel` | `Int → Channel@T` | Bounded channel; capacity ≥ 1 |
| `send` | `Channel@T → T → Null` | Send; suspend if buffer full |
| `recv` | `Channel@T → T` | Receive; suspend until available |
| `select-once` | `[Seq [Channel@T Fn]] → R` | Wait for first ready channel |
| `par` | `expr → T` | Spawn on thread pool immediately |
| `par-map` | `Fn → [Seq A] → [Seq B]` | Parallel map; results in order |
| `par-filter` | `Fn → [Seq A] → [Seq A]` | Parallel filter |
| `signal-channel` | `[Seq Signal] → Channel@Signal` | OS signal delivery channel |
| `timer-channel` | `ClockCap → Duration → Channel@Timestamp` | Periodic timer channel |
| `watch-channel` | `DirCap → Str → Channel@Null` | Filesystem watch channel |
| `tcp-listen` | `NetCap → Int → Channel@Handle` | Incoming TCP connections |
| `quic-listen` | `NetCap → Int → Channel@QuicConn` | Incoming QUIC connections |

### Change `src/parser.rs`

- Parser produces `SurfaceExpr` / `SurfaceDoc` / `SurfaceFile` instead of `Expr` / `Document` / `File`
- Assigns `NodeId` to every node at parse time from a per-parse counter
- `SurfaceDecl` nodes parsed separately from `SurfaceExpr` within document items

### Change `src/async_rt.rs`

- `run_program(fut)` — multi-thread runtime, work-stealing. Replaces `block_on`.
- `spawn_task(fut)` — `tokio::spawn` with `Arc`-based `TaskHandle`
- Thread-local `current_thread` runtime and `block_on` bridge removed

### Change test suite (`tests/`, `src/**/*_test.rs`)

- All tests become `#[tokio::test(flavor = "current_thread")]`
- Test helper `run_eval(source)` wraps evaluation in `run_program(...)`
- Individual test assertions unchanged

### Prelude tinct code

- `AstExpr`/`AstDoc`/`AstFile`/`AstParam`/`AstEntry`/`AstMatchArm`/`AstAnnotation`/`AstDecl`/`DocName` type declarations added
- `eval-document-pipeline`, `eval-file`, `eval-document-runtime`, `include` updated per §Updated Include-Decomp Tinct Code
- `async.llt` added: `exit`, `graceful-exit`, `finally`, `loop-select`, `retry`

### Delete

- `Expr`, `Document`, `File` from `src/ast.rs` — replaced by Surface/Core variants
- `dict_to_ast` from `src/ast_dict.rs` — removed entirely
- `dict_to_file` — never written
- `builtin_eval_ast` / `eval-ast` primitive — removed
- `eval_pipeline.rs` — `eval_file_with_input`, `eval_document`, `run_eval` all superseded (was deferred from include-decomp, deleted here)
- String-keyed `type:` Dict schema as AST output format

---

## Open Questions

**Q1 — `SurfaceExpr` NodeId in `Spanned<T>`:** Adding `id: NodeId` to `Spanned` is pervasive. Alternative: a separate `NodeIdMap` built during parsing that maps pointer identity to NodeId without touching `Spanned`. The pointer approach avoids the struct change but requires careful lifetime management. Lean toward adding `id` to `Spanned` — cleaner, no aliasing concerns.

**Q2 — Match payload allocation:** `surface_expr_match_view` materializes a shallow payload Dict per arm. Alternative: teach the match binder to destructure `Value::AstExpr` fields directly, avoiding that allocation. Defer; start with payload Dict, profile later.

**Q3 — `AstPattern` type declaration:** The `Pattern` enum (`Wildcard`, `Variable`, `Literal`, `TypeTag`, `Pin`, `Dict`, `Seq`, `Constructor`, `Or`) needs a full `AstPattern` tinct type declaration. Deferred to implementation sprint.

**Q4 — Lowering pass location:** Should lowering (`SurfaceExpr → CoreExpr`) happen inside the `eval` builtin (at call time), or as a separate phase that runs after `expand` and before `eval`? Inside `eval` is lazier — only expressions that get evaluated are lowered. A separate phase is more predictable. Lean toward inside `eval` to preserve the lazy model.

**Q5 — `deep-materialize` output format:** `Value::Variant` tree (nominal, type-checkable) or Dict with `type:` string keys (old schema)? `Value::Variant` is correct — the Dict schema is superseded.

**Q6 — `ResolutionTable` threading:** The resolver produces a `ResolutionTable`. The `eval` builtin receives `[Seq AstExpr]` from tinct code — how does it get the corresponding `ResolutionTable`? Option A: run resolution lazily inside the `lower` call when needed (resolution is deterministic and cheap to re-run). Option B: cache the `ResolutionTable` alongside the `AstFile` value. Lean toward A — avoid caching complexity; resolution is O(n) in expression count and fast.

**Q7 — `block_on` bridge for LSP:** LSP protocol handlers require synchronous callbacks in some implementations. Retain `block_on` at the LSP layer boundary only during the async migration; remove once the LSP event loop is fully async.

**Q8 — `dict?` on `Value::AstExpr`:** Returns `false` — these are nominal types, not plain Dicts. Code using `dict?` to gate AST access should use `type-of` or match instead.

---

## Prerequisites

- `include-decomp-primitives` complete (`blake3`, `cap-identity`, `load`, `include-cache-get`, `include-cache-put` registered)
- `include-decomp-eval-primitives` complete (interim `dict_to_file`-based `expand`/`eval`/`eval-types`; deletion of `builtin_include` and `eval_pipeline.rs` public functions; this proposal supersedes the representation but not the deletion work)
- `include-decomp-prelude` complete (self-hosted pipeline; this proposal refactors it)

---

## References

- Abelson, H. & Sussman, G.J. (1996). *Structure and Interpretation of Computer Programs*, 2nd ed. MIT Press. §4.1 "The Metacircular Evaluator." — homoiconic representation of code as data.
- Pombrio, J. & Krishnamurthi, S. (2014). "Hygienic Resugaring of Call-by-Value Evaluation Sequences." *ICFP 2014*. — origin tracking for desugared nodes (`Fn.desugared`, `SurfaceExpr` vs `CoreExpr` split motivation).
- Marlow, S. et al. (2009). "Runtime Support for Multicore Haskell." *ICFP '09*. — `par`/`seq` sparks and the GHC scheduler; the implicit-parallelism model for automatic parallel dict evaluation.
- Syme, D., Petricek, T. & Lomov, D. (2011). "The F# Asynchronous Programming Model." *PADL '11*. — Async workflows as first-class values; `task { }` computation expressions directly analogous to tinct's `[task ...]` builtin.
- Leijen, D., Schulte, W. & Burckhardt, S. (2009). "The Design of a Task Parallel Library." *OOPSLA '09*. — Structured task concurrency; `await-all`/`await-any` semantics.
- Go language specification. "Select statements." *go.dev/ref/spec*. — `select` over channels; tinct's `select-once` is directly analogous.
- Jones, S.P., Gordon, A. & Finne, S. (1996). "Concurrent Haskell." *POPL '96*. — MVars and the original "communicating lazy threads" model.
- Tokio documentation. "tokio::task::LocalSet." — The `!Send` cooperative execution model and multi-thread work-stealing runtime.
- Cardelli, L. (1997). "Type Systems." *Handbook of Computer Science and Engineering*. — nominal vs structural types and the value of nominal typing for AST node dispatch.
