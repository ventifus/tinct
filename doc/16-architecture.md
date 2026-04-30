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

### Iterative Evaluator — Defunctionalized CPS (CEK Machine)

> **Status:** Phase 1 (materialize) complete via iterative-eval-a — `materialize_rc` replaced by iterative `run()` loop with `Vec<Cont>` stack. Phase 2 (PendingCall lazy dispatch) complete via iterative-eval-b1 — `eval_call` returns PendingCall thunks. Phase 3 (access chains) complete via iterative-eval-b2 — DotAccessForce/BracketForceTarget continuations. Phase 4 (structural cleanup) complete via iterative-eval-b3 — MatCont→Cont rename, Action enum, run() function. eval() step conversion pending in iterative-eval-b4.

The iterative evaluator replaces the recursive `eval()` / `materialize()` call stack with an explicit continuation stack. The design follows Reynolds (1972) defunctionalization: each recursive call becomes a first-class `Cont` value pushed onto a `Vec<Cont>` stack. The main loop is a two-register machine `(action: Action, stack: Vec<Cont>)`.

**`Action` enum — what the machine does next:**

```rust
enum Action {
    Eval { expr: Rc<Spanned<Expr>>, env: Rc<RefCell<Environment>>, depth: usize },
    Materialize { thunk: Rc<Thunk>, mat_span: Option<Span>, depth: usize },
    Continue(Result<Value, Box<EvalError>>),    // result ready; pop top continuation and apply
}
```

**`Cont` enum — defunctionalized continuations (~18–20 variants):**

Each variant captures exactly the free variables needed to resume computation after its sub-expression completes. Large fields are `Box`ed to keep frame size ≤ 96 bytes.

```rust
enum Cont {
    // dict construction: remaining entries, dict_env built so far
    DictEntries {
        remaining: Box<Vec<(Spanned<Expr>, Spanned<Expr>)>>,
        dict_env: Rc<RefCell<Environment>>,
        dict_map: Box<IndexMap<Key, Rc<Thunk>>>,
    },

    // function call: force the function expression, then dispatch
    // Captures all free variables needed to complete the call after func is known.
    PendingCallForceFunc {
        thunk: Rc<Thunk>,     // the deferred function-position thunk being forced
        args: Box<Vec<Rc<Thunk>>>,
        named: Box<IndexMap<String, Rc<Thunk>>>,
        call_span: Span,
        depth: usize,
    },

    // access chain: remaining accesses after head is materialized
    AccessChain {
        remaining: Box<Vec<Spanned<Expr>>>,
        env: Rc<RefCell<Environment>>,
    },

    // document pipeline: bind result as $$ in child env, continue with next document
    DocumentPipeline {
        remaining: Box<Vec<Spanned<Expr>>>,
        env: Rc<RefCell<Environment>>,
    },

    // ... ~14 additional variants for $if branches, TypeAssert, Guarded validation,
    //     deep_materialize traversal, builtin arg forcing, etc.
}
```

**`PendingCallForceFunc` carries `named`:** The `named` field (`Box<IndexMap<String, Rc<Thunk>>>`) is required because named args are free variables of the continuation — they were bound at the call site and must survive until the function is forced and `bind_args_thunks` is called. Omitting `named` would silently discard named arguments, breaking the Kotlin-model call convention. (Reynolds 1972: defunctionalized continuations must capture all free variables of the original closure.)

**Main loop sketch:**

```rust
fn run(mut action: Action, mut stack: Vec<Cont>, ctx: &Rc<EvalContext>) -> EvalResult<Value> {
    loop {
        match action {
            Action::Eval { expr, env, depth } => {
                action = eval_step(expr, env, depth, &mut stack, ctx)?;
            }
            Action::Materialize { thunk, mat_span, depth } => {
                action = materialize_step(thunk, mat_span, depth, &mut stack, ctx)?;
            }
            Action::Continue(result) => {
                match stack.pop() {
                    None => return result,
                    Some(cont) => {
                        action = apply_cont(cont, result, &mut stack, ctx)?;
                    }
                }
            }
        }
    }
}
```

**Frame size discipline:** The `≤96B` budget keeps `Vec<Cont>` cache-friendly. Large fields (`Vec`, `IndexMap`, `Box<Spanned<Expr>>`) are heap-allocated via `Box`. The `Action` and `Cont` enums together represent the full CEK machine state; depth tracking becomes `stack.len()` (no separate counter needed, though `MAX_EVAL_DEPTH` can still be applied as `stack.len() > MAX_EVAL_DEPTH`).

**Relationship to current `ThunkState`:** `PendingBuiltin` and `PendingCall` in `ThunkState` are proto-continuations — defunctionalized call sites captured as data. The CEK machine subsumes them: `PendingBuiltin` becomes a `Cont::PendingBuiltinForceResult` continuation, `PendingCall` becomes `Cont::PendingCallForceFunc`. After CEK migration, `ThunkState` simplifies: `PendingBuiltin` and `PendingCall` are removed (now represented as `Cont` variants on the stack), leaving `{Unevaluated, InProgress, Materialized, Failed, Guarded}` — five states instead of seven. (`Guarded` is a current state added by the typeassert-structural sprint; it remains post-CEK to support proxy contract checking.)

### EvalContext — Evaluation Infrastructure Context

The evaluator threads an `EvalContext` through `eval()`, `materialize()`, and builtin dispatch. This separates evaluation infrastructure (file resolution, sandboxing) from variable bindings (`Environment`) and stack depth tracking (`depth`).

**Migration status:** Types defined and threaded (evalcontext-types sprint). Thread-local `INCLUDE_CTX` fully removed — no longer present in codebase.

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
    Builtin(fn(BuiltinArgs) -> Result<Rc<Thunk>, Error>),
    // BuiltinArgs<'a> { args: &'a [Rc<Thunk>], named: &'a IndexMap<String, Rc<Thunk>>,
    //                   depth: usize, call_span: Span, ctx: Rc<EvalContext> }
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

**Explicit materialization:** Use `$eval` to materialize a value eagerly at binding time. A syntax-level `[! expr]` force annotation is not part of the language — `$eval` serves this purpose.

### Performance Note: Operator Wrapper Overhead

The stdlib defines 12 LLT wrapper functions for the shadowable operators (`$<`, `$=`, `$+`, `$-`, `$*`, `$/`, `$if`, `$filter`, `$map`, `$reduce`, `$take`, `$drop`). Each wrapper function invocation adds:

- +1 `Rc<RefCell<Environment>>` allocation (the call environment)
- 2–3 environment insertions (binding the function arguments)
- +1 eval depth level per invocation

These costs are negligible for ordinary use but can accumulate in tight recursive loops that use operator builtins via their `$`-prefixed names (e.g., `[call $reduce $+ 0 $list]`).

**Prelude internal optimization:** The prelude itself uses `$builtin-add`, `$builtin-sub`, etc. (the raw Rust-native builtins registered in `standard_builtins()`) rather than the LLT wrapper aliases. This avoids the wrapper overhead for stdlib-internal implementations. User code that needs maximum throughput in hot paths can do the same.
