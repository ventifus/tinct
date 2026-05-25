# Architecture

## Components

```text
┌─────────────┐
│   Source    │  .llt file (documents separated by ---)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Parser    │  Text → AST (SurfaceProgram > SurfaceDocument > SurfaceNode > SurfaceExpression)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Desugar   │  Source-to-source AST transformation: rewrites _ implicit
│             │  lambdas to explicit [fn [_] ...] forms (src/desugar.rs)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Type Check │  Infer & verify types (advisory)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Evaluator  │  Per-document: scope chains, % pipeline, lazy
└──────┬──────┘
       │
       ▼
┌─────────────┐
│     CLI     │  Input parsing, $eval, output serialization
└─────────────┘
```

> **Note:** The Desugar pass is mandatory and must run after parsing and before both type checking and evaluation. Both downstream phases require `_` to be eliminated — skipping desugar causes the type checker to see `VarRef("_")` instead of `Fn` nodes, producing spurious "undefined variable _" errors.
>
> **Note:** The type checker runs after desugaring but type errors are advisory — evaluation proceeds regardless of type errors. This matches the design philosophy that types aid development without blocking execution.

### Implementation Architecture

**Pipeline phases:** Source text → Lexer → Parser (SurfaceProgram) → Desugar (SurfaceProgram) → Resolver (ResolutionTable) → TypeCheck (TypeAnnotationTable) → Lower (SurfaceNode → CoreExpr) → Eval (CoreExpr) → Output (Value)

**Key contracts:**

- `BuiltinFn` signature: `fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>>` (no `+ Send` — futures are `!Send`); `BuiltinArgs` carries owned `args: Vec<Arc<Thunk>>`, `named: Option<IndexMap<String, Arc<Thunk>>>`, `call_span: Span`, `ctx: Arc<EvalContext>`
- `Value` serialization: every `Value` variant must have handlers in both `value_to_json()` and `value_to_display_string()` (src/lib.rs)
- Type checker role: advisory only — type errors are warnings, evaluation proceeds regardless
- AST coverage: every `SurfaceExpression` variant requires both a `lower` handler (src/lower.rs producing `CoreExpr`) and a `typecheck` handler (src/typecheck.rs); every `CoreExpr` variant requires an `eval_core_expr` handler (src/eval.rs)
- Builtin registration: all builtins must appear in `standard_builtins()` (src/builtins.rs) — this is the authoritative list
- Environment chain: builtins → stdlib → user code (root env contains Rust-native builtins; stdlib env wraps root and loads prelude.llt; user code inherits from stdlib)
- Desugar ordering: `desugar_surface_program()` runs after parse and before both typecheck and eval in all entry points (eval_source, eval_surface_file_with_input, CLI, REPL, stdlib loading, lsp/document.rs::update_document)

**Cross-module coupling:**

- Circular dependency: builtins.rs calls `materialize`/`invoke_function` (eval.rs); eval.rs calls `standard_builtins()` (builtins.rs). Safe because dependency is at function-call level, not module init.
- Elaboration write-once: typecheck writes `TypeAssert.resolved_type` (RefCell) exactly once; re-typechecking the same AST panics. Parse a fresh AST for each typecheck run.
- Include cache: `EvalContext.state.include_cache` (HashMap keyed by file identity) memoizes `$include` results — same file included twice returns the cached thunk without re-evaluation.

### Iterative Evaluator — Defunctionalized CPS (CEK Machine)

> For the formal evaluation semantics (thunk lifecycle, materialization rules, laziness design), see [Evaluation](08-evaluation.md). For the type system extensions that interact with evaluation (TypeAssert contracts, row polymorphism), see [Type System Extensions](07-type-extensions.md).
>
> **Implementation note:** The iterative `run()` loop with `Vec<Cont>` stack drives all materialization. `eval_call` returns PendingCall thunks for lazy dispatch. DotAccess uses `DotAccessForce` continuations. TypeAssertCheck default expressions use Action::Eval. **One remaining recursive path:** `eval_recursive` (TypeAssert inner expression at eval_materialize.rs:1855 — needs a thunk without materializing, cannot use Action::Eval which goes through wrap_thunk). Modules: `eval_call.rs`, `eval_deep.rs`, `eval_access.rs`.

The iterative evaluator replaces the recursive `eval()` / `materialize()` call stack with an explicit continuation stack. The design follows Reynolds (1972) defunctionalization: each recursive call becomes a first-class `Cont` value pushed onto a `Vec<Cont>` stack. The main loop is a two-register machine `(action: Action, stack: Vec<Cont>)`.

**`Action` enum — what the machine does next:**

```rust
pub(crate) enum Action {
    /// Result ready — pop top continuation and apply, or return if stack empty
    Continue(EvalResult<Value>),
    /// Force this thunk to a materialized value
    Materialize {
        thunk: Arc<Thunk>,
        mat_span: Option<Span>,
    },
    /// Evaluate a CoreExpr to a thunk (wrapping, not forcing).
    ///
    /// Used by TypeAssert and Guarded default expression evaluation. Calls
    /// `eval_core_expr_pub` and wraps the result as `Action::Continue` (if already
    /// materialized) or `Action::Materialize` (if unevaluated). This variant replaces
    /// the old `Action::Eval { expr: Rc<Spanned<Expr>>, ... }` which required routing
    /// through `eval_step` and the Expr-based dispatch table.
    EvalCore {
        expr: Arc<Spanned<CoreExpr>>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
}
```

**`Cont` enum — defunctionalized continuations (~18–20 variants):**

Each variant captures exactly the free variables needed to resume computation after its sub-expression completes. Large fields are `Box`ed to keep frame size ≤ 96 bytes.

```rust
pub(crate) enum Cont {
    /// Memoize the result into the parent thunk. Used after materializing
    /// result thunks from PendingBuiltin/PendingCall/CoreExpr/Surface branches.
    Memoize(Box<MemoizeData>),
    /// Defunctionalized continuation for the PendingCall branch (Reynolds, 1972).
    /// After the function thunk is forced, this continuation inspects the
    /// resulting `Value::Function` or `Value::Builtin`, invokes it with the captured
    /// argument thunks, and pushes a `Memoize` continuation for the result thunk.
    PendingCallDispatch(Box<PendingCallDispatchData>),
    /// Defunctionalized continuation for the Guarded branch (Reynolds, 1972).
    /// After the inner thunk is forced, this continuation runs
    /// `validate_and_wrap_record` (for record types) or `value_matches_type` (for
    /// scalar types), then memoizes the validated value into `thunk`.
    GuardedValidate(Box<GuardedValidateData>),
    /// Resume a PendingBuiltin call after iteratively materializing arg[0].
    /// This prevents Rust stack growth from chains like $- → materialize → $- → ...
    /// where each builtin synchronously materializes its first arg. By pre-materializing
    /// arg[0] in the iterative loop, the chain stays on the continuation stack instead
    /// of the Rust call stack.
    BuiltinForceArg(Box<BuiltinForceArgData>),
    /// Access a field from a materialized dict. Pushed after target thunk is materialized.
    DotAccessForce(Box<DotAccessForceData>),
    /// Validate a materialized value against a TypeAssert annotation.
    /// Pushed by force_step's Expr::TypeAssert inline handler after evaluating the inner
    /// expression thunk; replaces the synchronous materialize() call that was the laziness
    /// violation in the TypeAssert branch.
    TypeAssertCheck(Box<TypeAssertCheckData>),
}

// Supporting data structures (large variants are boxed to keep Cont ≤96 bytes)

pub(crate) struct MemoizeData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) restore: Option<RestoreState>,
    pub(crate) ctx: Arc<EvalContext>,
}

pub(crate) struct PendingCallDispatchData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) func_thunk: Arc<Thunk>,
    pub(crate) args: Vec<Arc<Thunk>>,
    pub(crate) named: Option<Box<IndexMap<String, Arc<Thunk>>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
}

pub(crate) struct GuardedValidateData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) expected: Type,
    pub(crate) field_path: Vec<String>,
    pub(crate) guard_span: Span,
    pub(crate) inner_span: Span,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) blame_label: Option<crate::error::BlameLabel>,
    pub(crate) default: Option<GuardDefault>,
    pub(crate) restore: Option<RestoreState>,
}

pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) def: crate::value::BuiltinDef,
    pub(crate) args: Vec<Arc<Thunk>>,
    pub(crate) named: Option<IndexMap<String, Arc<Thunk>>>,
    pub(crate) call_span: Span,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) arg_idx: usize,
}

pub(crate) struct DotAccessForceData {
    pub(crate) field: crate::ast::DotKey,
    pub(crate) access_span: Span,
    pub(crate) target_def_span: Span,
    pub(crate) outer_mat_span: Option<Span>,
    pub(crate) ctx: Arc<EvalContext>,
}

pub(crate) struct TypeAssertCheckData {
    pub(crate) annotation: Box<Spanned<Annotation>>,
    pub(crate) resolved: Box<Option<Type>>,
    pub(crate) expr_span: Span,
    pub(crate) thunk_span: Span,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
}
```

**`PendingCallDispatchData` carries `named`:** The `named` field (`Option<Box<IndexMap<String, Arc<Thunk>>>>`) is required because named args are free variables of the continuation — they were bound at the call site and must survive until the function is materialized and `bind_args_thunks` is called. Omitting `named` would silently discard named arguments, breaking the Kotlin-model call convention. (Reynolds 1972: defunctionalized continuations must capture all free variables of the original closure.)

**Main loop sketch:**

```rust
async fn run(initial: Action, _ctx: &Arc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::EvalCore { expr, env, ctx } => {
                action = eval_core_expr_pub(&expr, env, &ctx, &mut stack).await;
            }
            Action::Materialize { thunk, mat_span } => {
                action = force_step(&thunk, mat_span, &mut stack, &ctx).await;
            }
            Action::Continue(result) => {
                match stack.pop() {
                    None => return result,
                    Some(cont) => {
                        action = apply_cont(cont, result, &mut stack, &ctx).await;
                    }
                }
            }
        }
    }
}
```

**Frame size discipline:** The `≤96B` budget keeps `Vec<Cont>` cache-friendly. Large fields (`Vec`, `IndexMap`, `Arc<Spanned<CoreExpr>>`) are heap-allocated via `Box`. The `Action` and `Cont` enums together represent the full CEK machine state; depth tracking becomes `stack.len()` (no separate counter needed; `MAX_CONTINUATION_STACK = 2048` is applied as `stack.len() > 2048`).

**Relationship to current `ThunkState`:** `PendingBuiltin` and `PendingCall` in `ThunkState` are proto-continuations — defunctionalized call sites captured as data. The CEK machine processes them via `Cont` variants (`Cont::BuiltinForceArg`, `Cont::PendingCallDispatch`) but does NOT remove them from ThunkState. PendingBuiltin and PendingCall are **permanent design elements** — they represent persistent deferred computation (lazy sequence steps, proxy handler dispatch) that cannot be converted to Unevaluated because builtin function pointers have no AST representation. The 7-state model (`{Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed}`) is the stable design, not a transitional artifact.

### EvalContext — Evaluation Infrastructure Context

The evaluator threads an `EvalContext` through `eval()`, `materialize()`, and builtin dispatch. This separates evaluation infrastructure (file resolution, sandboxing) from variable bindings (`Environment`).

EvalContext is defined and threaded throughout the evaluator. There is no thread-local `INCLUDE_CTX`. The iterative CEK machine uses heap-allocated continuations with no depth tracking.

**Config/State split:** EvalContext separates immutable session configuration from mutable evaluation state. Config is `Arc` (no Mutex) — the compiler enforces immutability. State is `Arc<Mutex>` for interior mutability.

```rust
struct EvalConfig {
    base_dir: cap_std::fs::Dir,
    stdlib_env: Arc<RwLock<Environment>>,
    no_fs: bool,
    require_integrity: bool,
}

struct EvalState {
    string_include_cache: HashMap<String, IncludeCacheEntry>,  // content-addressed include cache
    include_chain: Vec<(String, Span)>,
    eval_stack: Vec<(String, Span)>,
    class_registry: HashMap<String, RuntimeClassDecl>,
    // class_name interned via intern_class_name (&'static str); type_tags is Vec<String> (MPTC)
    instance_registry: HashMap<(&'static str, Vec<String>), Arc<Thunk>>,
    registered_classes: HashSet<String>,
}

struct EvalContext {
    config: Arc<EvalConfig>,         // shared, immutable
    state: Arc<Mutex<EvalState>>,    // shared, mutable
}
```

**What stays separate:**

- `Environment` — variable bindings and lexical scope chain. Created and nested per scope.

**Key invariant:** EvalContext is evaluation-session infrastructure; Environment is lexical scoping. A single EvalContext is shared across the entire evaluation of a file, while Environments are created per scope.

**Threading pattern:** `Arc<EvalContext>` — thunks capture `Arc::clone(&ctx)` at creation time and use it at materialization time. This is necessary because thunks are deferred (`Unevaluated`, `PendingBuiltin`, `PendingCall`) and materialized in a different stack frame than where they were created. Unlike `Environment` (which uses `Arc<RwLock<...>>`), EvalContext does not need an outer lock because it achieves interior mutability through its `state: Arc<Mutex<EvalState>>` field — the config is immutable by construction and only the state needs mutation.

**ThunkState captures EvalContext:** `Unevaluated`, `PendingBuiltin`, and `PendingCall` all store `ctx: Arc<EvalContext>` alongside their existing `env: Arc<RwLock<Environment>>`. When a thunk is materialized, it uses the captured context for include resolution, sandboxing, etc.

**BuiltinArgs:** Carries `ctx: Arc<EvalContext>` (was `Rc<EvalContext>` before the runtime-v2 sprint). Data is owned (not borrowed) so the struct can be moved into `Box<dyn Future>` (which has an implicit `'static` bound). Most builtins ignore ctx; `$include` and I/O builtins use it for include resolution and sandboxing. There is no `depth` field — the iterative CEK machine (see §Iterative Evaluator) uses heap-allocated continuations rather than tracking recursion depth.

**Public API:** `EvalContext`, `EvalConfig`, and `EvalState` are public. Callers construct an EvalContext and pass it to `eval_surface_file()`. Include context is passed as a parameter — no global set/clear functions.

**Per-caller patterns:**

- **CLI (main.rs):** Constructs EvalContext from CLI args (file path → base_dir), passes to eval_surface_file.
- **LSP:** Each DocumentState gets its own EvalContext. DocumentStore extracts base_dir from document URI. Config (stdlib_env) is shared across documents; state is per-document.
- **REPL:** Single EvalContext per session. Include state (guard, cache) persists across eval_input() calls. Session env accumulates bindings across commands. **Limitation:** `eval_input()` calls `parse_expression()` which returns the last expression of the FIRST document only; `---`-separated multi-doc input silently discards all documents after the first.

**Precedent:** Nix's `EvalState`, Nickel's `VirtualMachine`, Dhall's normalization context. Standard pattern in mature language implementations for separating evaluation infrastructure from variable bindings.

### Sketch: Value Enum

> **Note:** This sketch captures the original design intent. The authoritative implementation is in `src/value.rs`, `src/error.rs`, and `src/ast.rs`, which refine these types (e.g., `IndexMap` for insertion order, `Vec<Param>` for full parameter metadata, `Span` for source locations).

```rust
enum Value {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Dict(IndexMap<Key, Rc<Thunk>>),
    Seq(Rc<Thunk>, Rc<Thunk>),    // head, tail (tail evaluates to Seq or [] for end)
    Function {
        params: Vec<String>,
        body: AstNode,
        env: Environment,
    },
    Builtin(fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = Result<Arc<Thunk>, Error>>>>),
    // BuiltinArgs { args: Vec<Arc<Thunk>>, named: Option<IndexMap<String, Arc<Thunk>>>,
    //               call_span: Span, ctx: Arc<EvalContext> }
    // (updated for async — see src/value.rs for current type alias)
}

struct Thunk {
    expr: AstNode,
    env: Environment,
    state: RefCell<ThunkState>,
    source: SourceLocation,       // definition-site location
}

enum ThunkState {
    Unevaluated,
    PendingBuiltin(name, args),   // deferred builtin call
    PendingCall(func, args, named, call_span, caller_env, ctx),  // deferred function application (lazy $map, $update, etc.); caller_env captures caller's environment for default param evaluation
    InProgress,                   // cycle detection — hitting this during materialization means circular dep
    Materialized(Value),
    Failed(Box<EvalError>),       // error memoization — cached so re-access returns same error
    Guarded { inner, expected, field_path, guard_span },  // TypeAssert proxy contract — validates field types on access
}

struct SourceLocation {
    file: String,
    line: usize,
    column: usize,
}

enum Key {
    Int(i64),           // signed — negative integer keys are valid
    String(String),
}

struct Environment {
    bindings: HashMap<String, Rc<Thunk>>,  // lookup-only; insertion order carries no semantic meaning
    parent: Option<Rc<RefCell<Environment>>>,   // mutable — letrec needs self-referential bindings
}
```

### Compiler Notes: Strictness Analysis

**Materialization behavior is inferred by the compiler, not annotated in the type system.** The stdlib listing documents which functions are structural, lazy-transforming, materializing, or selective — but this is documentation for humans, not a language feature.

**Why not a type-level annotation:**

- Redundant — the annotation would restate what the code already does
- Fragile — refactoring internals could invalidate the annotation
- Over-simplified — real materialization behavior is conditional and nuanced (e.g., `$filter` materializes predicates but not passed-through values). No annotation captures "materialized only when the collection is non-empty."
- Burden — one more thing the programmer writes and maintains

**Compiler responsibilities:**

- **Demand analysis** — examine function bodies to determine which arguments are always materialized, sometimes materialized, or never materialized. Analogous to GHC's demand analyzer.
- **Builtin metadata** — builtins are implemented in Rust, so the compiler can't analyze their bodies. Materialization behavior must be manually declared as metadata on the Rust side.
- **Dead thunk detection** — warn when an expression is never materialized (dead code under lazy eval).

**Tooling integration:**

- LSP hover: show which arguments will be materialized at a call site
- LSP inlay hints: `[materialized]` / `[lazy]` next to arguments
- Auto-generated docs: annotate stdlib reference with materialization behavior

**Explicit materialization:** Use `$eval` to materialize a value eagerly at binding time. A syntax-level `[! expr]` eager-materialization annotation is not part of the language — `$eval` serves this purpose.

### Builtin Argument Strictness Annotations

Builtins declare per-position argument demand using `BuiltinDef`, a struct that replaces the bare `BuiltinFn` function pointer everywhere it appears. Strictness annotations follow Wadler & Hughes (1987) projections.

#### Strictness Enum

```rust
#[repr(u8)]           // 1-byte elements in pos_strictness slices; single byte load per check
#[non_exhaustive]     // future variants (e.g. Full for deep demand) won't break match arms
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    /// W&H "id" — identity projection. Argument never materialized at the dispatch site;
    /// the builtin receives the thunk and decides whether and when to materialize it.
    Id,

    /// W&H "seq" — materialize to head-normal form (WHNF) before the builtin is called.
    /// Note: the name "seq" derives from Haskell's `seq` combinator (Wadler & Hughes
    /// use "STR" on flat domains); it is unrelated to the `$seq` LLT builtin.
    /// Arithmetic, comparison, string, and numeric builtins are Seq in all positions.
    Seq,

    /// W&H spine projection — materialize the structural layer without element values.
    /// For Seq: materialize the outer thunk to Value::Seq(head_thunk, tail_thunk); head values
    /// stay lazy. For Dict: equivalent to Seq (WHNF already exposes the full key set).
    /// Used for collection arguments of dual-dispatch builtins ($map, $filter, etc.)
    /// where the type (Dict vs Seq) must be known but element values are not yet needed.
    ///
    /// Note: "Spine" is named by Peyton Jones & Partain (1993), who empirically
    /// confirmed that spine strictness is the dominant projection for list-consuming
    /// functions in real Haskell programs (~85–90% of strictness benefit comes from
    /// Seq alone; Spine covers the remainder for collection arguments). It is a derived
    /// concept in Wadler & Hughes (1987) rather than an explicitly named projection.
    ///
    /// W1 behavior: operationally identical to Seq at the dispatch site — one
    /// materialize() call materializes the collection to its outermost constructor. The
    /// Seq/Spine distinction exists for documentation accuracy and W2 code generation.
    Spine,
}
```

`BOT` (absent — argument never needed) and `BOTH`/`Full` (full recursive demand, equivalent to `deep_materialize`) are excluded: no tinct builtin has a truly dead argument, and full demand is a serialization concern, not a dispatch-site annotation.

#### BuiltinDef Struct

```rust
#[derive(Clone, Copy)]
pub struct BuiltinDef {
    /// The raw function pointer.
    pub func: BuiltinFn,
    /// Display name (also the LLT identifier without the `$` prefix).
    pub name: &'static str,
    /// Per-position argument demand. Empty slice means all arguments are `Id`.
    /// Positions beyond the slice length are implicitly `Id`.
    pub pos_strictness: &'static [Strictness],
}
```

`BuiltinDef` is `Copy` at zero cost: `func` is an 8-byte fn pointer, `name` and `pos_strictness` are fat pointers to static data. No heap allocation.

This struct replaces `BuiltinFn` at every site where a builtin is stored or dispatched:

```rust
// Value:
enum Value {
    Builtin(BuiltinDef),          // was: Builtin { name: &'static str, func: BuiltinFn }
}

// ThunkState:
ThunkState::PendingBuiltin {
    def: BuiltinDef,              // replaces separate name + func fields
    args: Box<Vec<Arc<Thunk>>>,
    named: Box<IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
    ctx: Arc<EvalContext>,
}
```

Strictness travels with the value. When the evaluator encounters `Value::Builtin(def)` or dispatches `ThunkState::PendingBuiltin { def, ... }`, `def.pos_strictness` is immediately available with no hash lookup and no secondary table. This matches the STG machine's info-table model (Peyton Jones 1992) and is required for efficient dispatch in the eval/apply model (Marlow & Peyton Jones 2004).

#### Registration

`standard_builtins()` returns `Vec<BuiltinDef>`. The `builtin!` macro gains an optional third argument for the strictness array. `builtin!("name", fn)` without the array implies all-`Id`. Because `pos_strictness` is `&'static [Strictness]`, the macro must expand the slice to a `const` item (not a temporary) to satisfy the `'static` lifetime bound:

```rust
// macro expansion for builtin!("+", builtin_add, [Seq, Seq]):
{
    const S: &[Strictness] = &[Strictness::Seq, Strictness::Seq];
    BuiltinDef { func: builtin_add, name: "+", pos_strictness: S }
}

builtin!("+", builtin_add,    [Seq, Seq])
builtin!("if", builtin_if,   [Seq, Id, Id])
builtin!("map", builtin_map, [Id, Spine])
builtin!("seq", builtin_seq)               // all-Id (no third arg, empty slice)
```

`create_root_env()` and the `builtin-*` operator aliases are updated to construct `BuiltinDef` entries. Alias entries carry the alias name (e.g. `"builtin-add"`) but share the same `pos_strictness` as the canonical builtin.

**Complete migration site inventory.** Every site that stores or deconstructs `name: &'static str` + `func: BuiltinFn` as separate builtin fields must be migrated atomically to `def: BuiltinDef`:

| Site | Location | Change |
|------|----------|--------|
| `Value::Builtin { name, func }` | `src/value.rs` | → `Value::Builtin(BuiltinDef)` |
| `ThunkState::PendingBuiltin { name, func, ... }` | `src/value.rs` | → `{ def: BuiltinDef, ... }` |
| `RestoreState::PendingBuiltin { name, func, ... }` | `src/eval_materialize.rs` | → `{ def: BuiltinDef, ... }` |
| `BuiltinForceArgData { builtin_name, func, ... }` | `src/eval_materialize.rs` | → `{ def: BuiltinDef, arg_idx: usize, ... }` |
| `take_pending_builtin()` return tuple | `src/value.rs` | `(&str, BuiltinFn, ...)` → `(BuiltinDef, ...)` |
| `Thunk::new_pending_builtin(name, func, ...)` | `src/value.rs` | → `new_pending_builtin(def, ...)` |
| `create_root_env()` aliases `Vec<(&str, BuiltinFn)>` | `src/builtins.rs` | → `Vec<BuiltinDef>` |

All seven sites must change together — a partial migration leaves mismatched field accesses that do not compile.

**Value enum size.** `Value::Builtin` grows from 24 bytes (`name` 16 + `func` 8) to 40 bytes (adding `pos_strictness` 16). Add a compile-time assertion after the migration to catch future regressions:

```rust
const _: () = assert!(std::mem::size_of::<Value>() == EXPECTED);
```

Verify `Value`'s dominant variant (`Value::Dict`) still determines the enum size before adding the assertion.

#### Strictness Annotation Table

S = Seq, I = Id, Sp = Spine

| Builtin | Strictness | Notes |
|---------|-----------|-------|
| `+`, `-`, `*`, `/` | [S, S] | Both operands always materialized |
| `=`, `<` | [S, S] | Both operands materialized for comparison |
| `if` | [S, I, I] | Condition only; branches returned as thunks |
| `str`, `upper`, `lower`, `trim` | [S] | String materialized for operation |
| `split` | [S, S] | String and separator |
| `replace` | [S, S, S] | String, pattern, replacement |
| `floor`, `round` | [S] | Numeric materialized |
| `to-int`, `to-float` | [S] | Conversion |
| `eval` | [S] | Triggers materialization explicitly |
| `error` | [S] | Message materialized for display |
| `try` | [I] | 1-arg; function thunk deferred; `try` evaluates it internally catching errors |
| `apply` | [S, S] | Function and args-dict both materialized |
| `until` | [I, I, I] | pred, f, init applied lazily per iteration |
| `type-of`, `seq?` | [S] | Type inspection requires WHNF |
| `from-json` | [S] | String parsed |
| `include` | [S] | Path materialized; hash named arg is `Id` (excluded) |
| `seq` | [I, I] | Both head and tail deferred (constructor) |
| `head`, `tail` | [S] | Seq materialized to expose structure |
| `collect` | [Sp] | Spine materialized; element values materialized by builtin loop |
| `range` | [S, S] | Both bounds materialized |
| `repeat` | [I] | Element deferred (infinite repetition without materializing) |
| `cycle` | [Sp] | Base collection spine traversed |
| `iterate`, `unfold` | [I, I] | Function and seed deferred per-step |
| `keys` | [Sp] | Dict spine materialized for key enumeration |
| `length` | [Sp] | Spine materialized for count |
| `merge` | [I, I] | Constructs Value::Overlay without materializing either arg; pre-materializing would change error-surfacing semantics (detectable via `$try`) |
| `append` | [S, I] | arg[0] (target dict) materialized; arg[1] (value to append) inserted as thunk, preserving laziness |
| `map`, `filter` | [I, Sp] | fn/pred lazy; collection spine materialized for type dispatch |
| `take`, `drop` | [S, Sp] | n materialized; collection spine materialized for dispatch |
| `reduce` | [I, I, Sp] | f and init lazy; collection spine materialized for dispatch |
| `join` | [S, Sp] | Separator materialized; collection spine materialized; elements materialized by builtin |
| `concat` | [Sp, S] | First collection spine materialized for dispatch; second always materialized by builtin |
| `sort`, `reverse`, `rest` | [Sp] | Sequence structure materialized |
| `cons` | [I, Sp] | Element deferred; collection spine materialized for dispatch |
| `proxy` | [I] | Wraps thunk without materializing |

#### W1: Dispatch-Time Materialization

**Implementation via `BuiltinForceArgData` generalization.** The CEK machine is iterative and cannot pre-materialize multiple arguments in a single step. W1 generalizes the existing `Cont::BuiltinForceArg` continuation (which already pre-materializes `args[0]` unconditionally) to cover all `Seq`/`Spine` positions:

1. `BuiltinForceArgData` gains an `arg_idx: usize` field tracking which position is currently being materialized.
2. When `PendingBuiltin { def, args, ... }` is dispatched, instead of immediately building `BuiltinArgs`, the dispatcher finds the first `Seq`/`Spine` position in `def.pos_strictness`, pushes `Cont::BuiltinForceArg { def, args, named, ..., arg_idx: i }`, and returns `Action::Materialize { thunk: args[i] }`.
3. In `apply_cont` for `Cont::BuiltinForceArg`, after the arg at `arg_idx` is materialized, find the next `Seq`/`Spine` position. If one exists, push another `Cont::BuiltinForceArg` with the incremented index. If none remain, construct `BuiltinArgs` and call `def.func`.
4. The existing `builtin_name == "apply"` string comparison at `eval_materialize.rs:1114` is deleted — it becomes a specific instance of this general mechanism, since `$apply` is annotated `[Seq, Seq]`.

Because `materialize()` updates the thunk's `RefCell<ThunkState>` in place, the builtin's own subsequent `materialize(args[i])` call is a no-op read after W1 has materialized it. This eliminates the per-argument state-machine cost for `Seq` and `Spine` positions.

Error propagation: if pre-materialization of any `Seq`/`Spine` argument fails, the error surfaces before the builtin executes — preserving sequential error semantics and making the error site predictable regardless of the builtin's own argument access order.

#### W2: Call-Creation Time (Future)

When the function expression in a call is a `VarRef` that immediately resolves to `Value::Builtin(def)`, evaluate `Seq`-position argument expressions eagerly (inline eval rather than `Thunk::new_unevaluated`), creating `Thunk::new_materialized(value)`. This eliminates the `Rc<Thunk>` allocation for `Seq` args on common hot paths like `[call $+ $x $y]`. Gated on: confirmed benchmark showing allocation savings exceed early-resolution overhead; arena migration (which changes the cost model for thunk creation).

### Performance Note: Operator Wrapper Overhead

The stdlib defines 12 LLT wrapper functions for the shadowable operators (`$<`, `$=`, `$+`, `$-`, `$*`, `$/`, `$if`, `$filter`, `$map`, `$reduce`, `$take`, `$drop`). Each wrapper function invocation adds:

- +1 `Rc<RefCell<Environment>>` allocation (the call environment)
- 2–3 environment insertions (binding the function arguments)
- +1 eval depth level per invocation

These costs are negligible for ordinary use but can accumulate in tight recursive loops that use operator builtins via their `$`-prefixed names (e.g., `[call $reduce $+ 0 $list]`).

**Prelude internal optimization:** The prelude itself uses `$builtin-add`, `$builtin-sub`, etc. (the raw Rust-native builtins registered in `standard_builtins()`) rather than the LLT wrapper aliases. This avoids the wrapper overhead for stdlib-internal implementations. User code that needs maximum throughput in hot paths can do the same.

### Performance Characteristics

**Hot path allocation patterns:**

- **Environment lookup is O(depth)**: `Environment::get()` walks the parent chain on every variable reference. Deeply nested scopes compound this cost.
- **IndexMap ~20% slower than HashMap**: Dict operations use `IndexMap` to preserve insertion order (required for dict semantics). Type-level `Substitution.type_map`/`row_map` also use `IndexMap` but could be `HashMap` (order irrelevant).
- **Thunk boxing cost**: Every value is wrapped in `Rc<RefCell<ThunkState>>`. Lazy evaluation requires this indirection but adds allocation and refcounting overhead.
- **Substitution::apply() is O(type_size)**: Type inference calls `apply()` per unification. Each call allocates a `HashSet<String>` for cycle detection and walks the entire type tree.

**Known bottlenecks:**

- Rc clone frequency in dict construction loops
- AST deep-clone per call argument (until AST nodes become Rc)
- Type tree traversal during multi-pass dict inference

## Security & Threat Model

### Trust Boundaries

LLT source files are **untrusted input**. The parser, type checker, and evaluator must handle malicious or pathological input gracefully without crashing, exhausting system resources, or allowing unauthorized file system access. Trust boundaries include:

- **Developer-facing**: LLT files in version control, build tools, CI pipelines
- **End-user-facing**: LSP server processing document content from editors
- **Embedded use**: LLT runtime embedded in other applications

### Current Security Posture

**What IS restricted:**

| Resource | Limit | Enforcement Point | Rationale |
|----------|-------|------------------|-----------|
| **Parse depth** | `MAX_PARSE_DEPTH = 256` | `src/parser.rs:42` | Prevents stack exhaustion from deeply nested syntax (iterative parser with explicit depth counter) |
| **Lexer depth** | `MAX_LEX_DEPTH = 256` | `src/lexer.rs:106` | Prevents stack overflow from deeply nested bracket expressions |
| **Type inference** | `MAX_SUBST_SIZE = 50,000` | `src/types.rs:386` | Prevents O(N²) type inference DoS from deeply chained dot-accesses |
| **Type unification** | `MAX_APPLY_DEPTH = 256` | `src/types.rs:382` | Caps substitution application depth to prevent exponential blowup |
| **File size** | `MAX_FILE_SIZE = 10 MB` | `src/builtins.rs:47` | Caps `$include` file reads and LSP document size |
| **Collection size** | `MAX_COLLECT_SIZE = 1,000,000` | `src/builtins.rs:36` | Prevents memory exhaustion from `$collect` on infinite sequences |
| **String size** | `MAX_STRING_SIZE = 64 MB` | `src/builtins.rs:40` | Caps string output from `$replace`, `$upper`, `$lower`, `$join` |
| **Split parts** | `MAX_SPLIT_PARTS = 1,000,000` | `src/builtins_string.rs:23` | Prevents memory exhaustion from adversarial `$split` patterns |
| **LSP document size** | `MAX_DOCUMENT_SIZE = 10 MB` | `src/lsp/server.rs:22` | Rejects oversized documents before parsing (equals `MAX_FILE_SIZE`) |
| **LSP method names** | `MAX_METHOD_NAME_LEN = 256` | `src/lsp/server.rs:33` | Prevents pathological LSP method name allocation |
| **Continuation stack** | `MAX_CONTINUATION_STACK = 2048` | `src/eval_materialize.rs` | Bounds iterative CEK machine continuation depth; prevents unbounded stack growth |
| **File I/O** | `--no-fs` flag, LSP default | `src/main.rs:39`, `src/lsp/document.rs:109` | Disables `$include` and `$from-json` file reads; LSP enables by default (CWE-22 mitigation) |
| **Eval timeout** | `--timeout` flag (Unix only) | `src/main.rs:43` | Wall-clock timeout with SIGALRM; exits with code 2 on expiry |

**Note:** Evaluation depth is bounded by the iterative CEK machine's continuation stack (`MAX_CONTINUATION_STACK = 2048`), cycle detection (`InProgress` sentinel), and parser depth limit (`MAX_PARSE_DEPTH = 256`). The old recursive evaluator with `MAX_EVAL_DEPTH = 256` was replaced by the iterative CEK machine in the runtime-v2 migration.

**What is NOT restricted:**

- **CPU time**: No hard limit by default except `--timeout` flag on Unix platforms
- **Memory**: No heap usage cap; bounded only by collection/string/file size limits
- **Network**: Controlled via `NetCap` capability — programs only reach hosts and ports explicitly granted by `--cap-net` flags; the builtin layer enforces the allowlist before any socket is opened

### Mitigations in Place

1. **Integer overflow**: All arithmetic builtins (`$+`, `$-`, `$*`, `$/`) use checked arithmetic (`checked_add`, `checked_sub`, `checked_mul`) to prevent silent wraparound in release mode
2. **Cycle detection**: `InProgress` thunk state sentinel prevents infinite loops from circular data structures
3. **Error memoization**: Failed thunks cache errors in `ThunkState::Failed` to prevent repeated evaluation of broken computations
4. **Depth tracking**: All recursive eval/materialize/typecheck paths check depth limits **before** recursion, not after
5. **LSP crash prevention**: Document size limit, method name cap, `no_fs=true` by default, panic-safe error handling
6. **Kernel-level sandboxing**: rlimits, Landlock filesystem ACLs, and seccomp-bpf syscall filtering (see §Implemented Kernel-Level Sandboxing)

### Implemented Kernel-Level Sandboxing

The following kernel-level security features are implemented in `src/main.rs` and documented in [Tooling](12-tooling.md):

- **rlimit resource caps** (`src/main.rs:447`): `RLIMIT_AS` (virtual memory, default 512 MB), `RLIMIT_CPU` (CPU time via `--max-cpu`), `RLIMIT_NOFILE` (file descriptors, default 64). Applied early in startup before evaluation begins. Unix-only; flags accepted on other platforms for CLI compatibility but have no effect.
- **Landlock filesystem ACLs** (`src/main.rs:639`, Linux 5.13+): Auto-triggered when `--cap-fs` entries are present. Confines filesystem access to `--cap-fs` directory trees at the kernel level. Graceful degradation on older kernels. Defense-in-depth: catches unauthorized paths at the kernel level even if cap-std or DirCap handling has bugs. Disabled with `--no-landlock`.
- **seccomp-bpf syscall filtering** (`src/main.rs:541`, Linux only): Blocks network syscalls (`socket`, `connect`, `bind`, `listen`, `accept`) and process creation (`fork`, `clone`, `execve`) unless `--cap-net` is set. Graceful degradation: if seccomp cannot be applied, a warning is printed and evaluation continues.

### Security Hardening

The following security features are implemented:

- **Import integrity hashes**: `$include` with optional hash verification (Dhall-inspired) to detect file tampering; `--require-integrity` flag to enforce hashes on all includes
- **File descriptor-based `$include`**: Eliminates TOCTOU race (canonicalize → metadata → read) by using `cap-std` for fd-based path resolution with `RESOLVE_BENEATH` semantics
- **Dependency scanning**: `cargo audit` as CI gate to surface RustSec advisories before they accumulate

### Attack Surface Analysis

**DoS via crafted inputs** (mitigated):

- Deep nesting: MAX_PARSE_DEPTH, MAX_LEX_DEPTH enforce limits before stack overflow
- Infinite sequences: MAX_COLLECT_SIZE bounds materialization; lazy evaluation makes unbounded data safe
- Type inference explosion: MAX_SUBST_SIZE caps substitution growth from pathological type annotations
- String amplification: MAX_STRING_SIZE caps output from `$replace`, `$join`, `$upper`, `$lower`

**Path traversal** (partially mitigated):

- LSP mode: `no_fs=true` disables all file I/O, preventing CWE-22 attacks via malicious document content
- CLI mode: `$include` uses `canonicalize()` to resolve symlinks and relative paths but has no root confinement; `--no-fs` flag disables file I/O entirely
- TOCTOU race: canonicalize → metadata → read creates race window; cap-std fd-based reads eliminate this race

**Panic hygiene**:

- All user-reachable code paths return `Err(...)`, not `panic!()`
- Two `expect("collection too large")` sites remain on index casts after MAX_COLLECT_SIZE check
- `unsafe` blocks limited to SIGALRM handler setup and alarm cancellation (`src/main.rs:168,176-190,302-304`) — audited and sound

**Dependency hygiene**:

- All dependencies are actively maintained stable crates (clap, indexmap, serde_json, lsp-server, lsp-types, rustyline)
- No known CVEs
- `cargo audit` is automated in CI

## Testing Strategy

Tinct uses a multi-layer testing approach that matches the component architecture. Each layer has its own testing discipline:

**Unit tests** (per module, ~1000+ tests total):

- Parser unit tests in `src/parser.rs` — test module at bottom of file
- Evaluator unit tests in `src/eval.rs` — thunk lifecycle, state transitions, depth limits, error caching
- Type checker unit tests in `src/types.rs` — unification, substitution, occurs check, row polymorphism
- Builtin unit tests in `src/builtins.rs` — argument validation, error paths, edge cases

**Corpus tests** (end-to-end, ~200+ tests):

- `tests/corpus/eval/<category>/` — evaluation tests (parse + desugar + eval), output matches expected
- `tests/corpus/parse/<category>/` — parse-only tests, AST or error matches expected
- Format: `.llt-eval` and `.llt-parse` files with `===` delimiter between input and expected output (see [Tooling](12-tooling.md) §Corpus Test Format)
- Coverage: all language features, edge cases, error conditions

**CLI integration tests** (REPL and LSP):

- REPL session tests — multi-command interactions, environment persistence, `$eval` behavior
- LSP protocol tests — document sync, hover, diagnostics, incremental updates
- File I/O sandboxing tests — `--no-fs` flag, include guards, path canonicalization

**Testing philosophy**:

- **Unit tests** — per module isolation (src/parser.rs, src/eval.rs, src/types.rs, src/builtins.rs). Test individual functions, error paths, edge cases, state transitions.
- **Corpus tests** — end-to-end validation (tests/corpus/valid/, tests/corpus/invalid/). Test language features in combination; verify error messages match expected output.
- **CLI integration tests** — REPL session tests, LSP protocol tests, file I/O sandboxing tests (--no-fs flag, include guards).
- **Coverage invariant** — new language features require BOTH unit tests (isolated) and corpus tests (end-to-end). Error paths must have corpus tests with substring matching.
- **Depth limit discipline** — tests that exercise MAX_EVAL_DEPTH must use 16MB stack threads to avoid Rust stack overflow before LLT depth limit is reached.
