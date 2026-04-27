# Architecture

### Components

```
┌─────────────┐
│   Source    │  .llt file (documents separated by ---)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Parser    │  Text → AST (File > Document > Expr)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Desugar   │  Source-to-source AST transformation: rewrites $_ implicit
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
│  Evaluator  │  Per-document: scope chains, $$ pipeline, lazy
└──────┬──────┘
       │
       ▼
┌─────────────┐
│     CLI     │  Input parsing, $eval, output serialization
└─────────────┘
```

> **Note:** The Desugar pass is mandatory and must run after parsing and before both type checking and evaluation. Both downstream phases assume `$_` has been eliminated — skipping desugar causes the type checker to see `VarRef("_")` instead of `Fn` nodes, producing spurious "undefined variable _" errors.

> **Note:** The type checker runs after desugaring but type errors are advisory — evaluation proceeds regardless of type errors. This matches the design philosophy that types aid development without blocking execution.

### EvalContext — Evaluation Infrastructure Context

The evaluator threads an `EvalContext` through `eval()`, `materialize()`, and builtin dispatch. This separates evaluation infrastructure (file resolution, sandboxing) from variable bindings (`Environment`) and stack depth tracking (`depth`).

**Design decision:** EvalContext replaces the thread-local `INCLUDE_CTX` pattern. Thread-locals create invisible coupling, prevent multi-file LSP support (each document needs its own include context), and require fragile set/clear ceremonies at every call site.

**Config/State split:** EvalContext separates immutable session configuration from mutable evaluation state. Config is `Rc` (no RefCell) — the compiler enforces immutability. State is `Rc<RefCell>` for interior mutability.

```rust
struct EvalConfig {
    base_dir: PathBuf,
    stdlib_env: Rc<RefCell<Environment>>,
    no_fs: bool,
    // future: allowed_paths (cap-std include-fd-hardening sprint)
}

struct EvalState {
    include_guard: HashSet<PathBuf>,
    include_cache: HashMap<PathBuf, Rc<Thunk>>,
    // future: trace_log, eval_stats
}

struct EvalContext {
    config: Rc<EvalConfig>,         // shared, immutable
    state: Rc<RefCell<EvalState>>,   // shared, mutable
}
```

**What stays separate:**
- `depth: usize` — stack-depth counter, passed by value and incremented per recursive call (`eval(expr, env, ctx, depth + 1)`). Not session state — it's naturally fork-friendly for parallel evaluation paths.
- `Environment` — variable bindings and lexical scope chain. Created and nested per scope.

**Key invariant:** EvalContext is evaluation-session infrastructure; Environment is lexical scoping; depth is call-stack tracking. A single EvalContext is shared across the entire evaluation of a file, while Environments are created per scope and depth increments per recursive call.

**Threading pattern:** `Rc<EvalContext>` — thunks capture `Rc::clone(&ctx)` at creation time and use it at materialization time. This is necessary because thunks are deferred (`Unevaluated`, `PendingBuiltin`, `PendingCall`) and materialized in a different stack frame than where they were created. Unlike `Environment` (which uses `Rc<RefCell<...>>`), EvalContext does not need an outer RefCell because it achieves interior mutability through its `state: Rc<RefCell<EvalState>>` field — the config is immutable by construction and only the state needs mutation.

**ThunkState captures EvalContext:** `Unevaluated`, `PendingBuiltin`, and `PendingCall` all store `ctx: Rc<EvalContext>` alongside their existing `env: Rc<RefCell<Environment>>`. When a thunk is forced, it uses the captured context for include resolution, sandboxing, etc.

**BuiltinArgs:** Gains a `ctx: Rc<EvalContext>` field. The existing `depth: usize` field remains (call-site depth, captured at PendingBuiltin creation time). Most builtins ignore ctx; only `$include` and future I/O builtins use it.

**Public API:** `EvalContext`, `EvalConfig`, and `EvalState` are public. Callers construct an EvalContext and pass it to `eval_file()`. The `set_include_context()` / `clear_include_context()` functions are removed — the fragile set/clear ceremony is replaced by straightforward parameter passing.

**Per-caller patterns:**
- **CLI (main.rs):** Constructs EvalContext from CLI args (file path → base_dir), passes to eval_file.
- **LSP:** Each DocumentState gets its own EvalContext. DocumentStore extracts base_dir from document URI. Config (stdlib_env) is shared across documents; state is per-document.
- **REPL:** Single EvalContext per session. Include state (guard, cache) persists across eval_input() calls. Session env accumulates bindings across commands.

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
    Builtin(fn(BuiltinArgs) -> Result<Rc<Thunk>, Error>),
    // BuiltinArgs { positional: Vec<Rc<Thunk>>, named: IndexMap<String, Rc<Thunk>> }
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
    PendingCall(func, args),      // deferred function application (lazy $map, $update, etc.)
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
    bindings: HashMap<String, Rc<Thunk>>,
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

**Explicit materialization:** Use `$eval` to materialize a value eagerly at binding time. A syntax-level `[! expr]` force annotation is not part of the language — `$eval` serves this purpose.
