# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger



## Arena Allocation: Arena-Based Thunks and Flat Environments

Replace `Rc<Thunk>` / `Rc<RefCell<Environment>>` with arena-allocated thunks and flat environments for O(1) variable lookup, zero Rc overhead, and section-scoped bulk deallocation. See doc/08-evaluation.md §Allocation Strategy and doc/whatif/arena-patterns.md.

Design decision (DONE.md): ship Phase 1+2 together as a single migration — variable resolution pass + ThunkArena + EnvArena. Starting with Dict alone creates a hybrid model requiring a second migration.

### arena-resolve: Variable Resolution Pass

Pre-eval analysis pass assigns `(level, slot)` pairs to `VarRef` nodes. Prerequisite for flat environments — without slot indices, `FlatEnv` can't do O(1) lookup. See doc/whatif/arena-patterns.md §Variable Resolution Pass Design.

- [ ] Add resolution cache to `Expr::VarRef` — extend from `VarRef(String)` to carry an optional `(u32, u32)` level/slot pair; `None` for unresolved (computed keys, `$include`-introduced bindings) (`src/ast.rs`)
- [ ] Implement `Resolver` struct with scope stack `Vec<HashMap<String, u32>>` — `enter_dict(static_keys)` pushes scope, `exit_dict()` pops, `resolve(name) -> Option<(u32, u32)>` searches stack in reverse (`src/resolve.rs`)
- [ ] Walk AST to populate resolution cache on all VarRef nodes — handle dict entries (push scope with static keys), fn params (push scope), nested expressions (recurse); leave `None` for computed-key and `$include`-introduced bindings (`src/resolve.rs`)
- [ ] Wire resolution pass into pipeline: call after parsing, before type checking and evaluation; update `eval_source()`, `eval_file()`, `eval_file_with_input()` (`src/lib.rs`)
- [ ] Update `eval` VarRef case to use resolved `(level, slot)` when present — O(1) slot lookup via `env.get_slot(level, slot)`, falling back to name lookup for `None` (`src/eval.rs`)
- [ ] Unit tests: static key resolution, nested scope shadowing, fn param resolution, computed key remains `None`, `%` pipeline variable resolves correctly (`src/resolve.rs`)
- [ ] Verify full corpus test suite passes unchanged — resolution is transparent to evaluation semantics (`tests/`)

### arena-types: Arena Type Definitions

Introduce `ThunkId`, `EnvId`, `ThunkArena`, `EnvArena`, `FlatEnv` types with letrec allocation pattern. See doc/whatif/arena-patterns.md §Design.

**Depends on:** `arena-resolve`

- [ ] Add `ThunkId(u32)` newtype (`Copy, Clone, Debug, PartialEq, Eq, Hash`) and `ThunkArena` struct (`Vec<Thunk>`, `alloc() -> ThunkId`, `get(ThunkId) -> &Thunk`) (`src/arena.rs`)
- [ ] Add `EnvId(u32)` newtype and `EnvArena` struct (`Vec<FlatEnv>`, `alloc() -> EnvId`, `get(EnvId) -> &FlatEnv`) (`src/arena.rs`)
- [ ] Add `FlatEnv` struct: `slots: Vec<ThunkId>` (static keys indexed by compile-time slot), `overflow: HashMap<String, ThunkId>` (computed keys), `parent: Option<EnvId>` (stdlib root chain only) (`src/arena.rs`)
- [ ] Implement letrec allocation pattern: `ThunkArena::alloc_placeholder()` returns `ThunkId` pointing at sentinel thunk; `arena.get(id).set_state(...)` fills via existing `RefCell` interior mutability (`src/arena.rs`)
- [ ] Unit tests: alloc/get, placeholder + fill lifecycle, letrec self-reference pattern, FlatEnv slot lookup + overflow fallback, EnvArena parent chain traversal (`src/arena.rs`)

### arena-eval: Evaluator Migration to Arena

Migrate Value, ThunkState, BuiltinFn, eval, and builtins from `Rc<Thunk>` to `ThunkId`/`EnvId`. See doc/08-evaluation.md §Allocation Strategy Phase 2.

**Depends on:** `arena-types`

- [ ] Change `Value` variants to arena handles: `Dict(IndexMap<Key, ThunkId>)`, `Function { ..., env: EnvId }`, `Seq { head: ThunkId, tail: ThunkId }`, `Proxy { handler: ThunkId }` (`src/value.rs`)
- [ ] Change `ThunkState` variants (`Unevaluated`, `PendingCall`, `PendingBuiltin`, `Guarded`) to use `ThunkId`/`EnvId` instead of `Rc<Thunk>`/`Rc<RefCell<Environment>>` (`src/value.rs`)
- [ ] Add `ThunkArena` + `EnvArena` to `EvalContext`; change `BuiltinFn` signature to receive arena access (`src/builtins.rs`, `src/eval.rs`)
- [ ] Update `eval()`, `materialize()`, and `deep_materialize()` to allocate via arena and access thunks via `ThunkId` (`src/eval.rs`, `src/eval_materialize.rs`)
- [ ] Update all builtins to use `ThunkId`/`EnvId` — arithmetic, string ops, collection ops, control flow, I/O (`src/builtins.rs`, `src/builtins_string.rs`)
- [ ] Update public API functions: `eval_source()`, `eval_file()`, `eval_file_with_input()`, `value_to_json()`, `value_to_display_string()` (`src/lib.rs`)
- [ ] Update REPL to create arena per input evaluation (`src/repl.rs`)
- [ ] Update LSP document evaluation to use arena per document (`src/lsp/document.rs`)
- [ ] Verify full test suite passes — corpus tests, unit tests, CLI tests (`tests/`)

### arena-cek: CEK Machine Integration

Convert the recursive evaluator to an iterative CEK machine loop with arena-allocated state. See doc/16-architecture.md §Iterative Evaluator.

**Depends on:** `arena-eval`

- [ ] Define `Action` enum (`Eval(ExprId, EnvId)` / `Materialize(ThunkId)` / `Continue(Value)`) and `Cont` enum with `ThunkId`/`EnvId` handles (`src/eval.rs`)
- [ ] Implement iterative two-register loop: `action` register + `cont_stack: Vec<Cont>` — arena is a field of the machine state (`src/eval.rs`)
- [ ] Convert `eval()` call sites from recursive function calls to `Action::Eval` pushes onto the continuation stack (`src/eval.rs`)
- [ ] Convert `materialize()` integration — existing iterative `materialize_rc()` becomes a sub-loop within the CEK machine (`src/eval_materialize.rs`)
- [ ] Update tail-call detection: self-recursive calls in tail position reuse the current frame instead of pushing a new `Cont` (`src/eval.rs`)
- [ ] Performance comparison: benchmark recursive vs CEK on deeply nested and wide dict workloads; verify no regression on shallow workloads (`benches/`)
- [ ] Verify full test suite passes (`tests/`)

### arena-migrate: Selective Migration at `---` Boundaries

Implement the migration algorithm that translates arena-allocated thunks to `Rc`-backed persistent storage at `---` boundaries. See doc/08-evaluation.md §Allocation Strategy (migration algorithm).

**Depends on:** `arena-eval`

- [ ] Implement `migrate(value, arena, thunk_table, env_table) -> Rc<Thunk>` — trace from `%` result, rewrite `ThunkId` to `Rc<Thunk>` with two translation tables for identity preservation (`src/arena.rs`)
- [ ] Handle all `ThunkState` variants in migration: `Materialized` (recurse into value), `Unevaluated` (migrate env), `PendingBuiltin`/`PendingCall` (migrate args), `Failed` (copy error), `InProgress` (unreachable at `---`) (`src/arena.rs`)
- [ ] Two translation tables: `HashMap<ThunkId, Rc<Thunk>>` + `HashMap<EnvId, Rc<RefCell<Environment>>>` — insert placeholder before recursing to handle letrec cycles (`src/arena.rs`)
- [ ] Wire migration at `---` boundary in document evaluation: migrate `%`-reachable thunks, bind migrated result as `%` for next section, drop section arena (`src/eval.rs`)
- [ ] Handle `$include` cache interaction: include cache stores `Rc`-backed values (arena-independent); `$include` results within a section are Rc handles embedded in the arena, not arena-allocated (`src/builtins.rs`)
- [ ] Unit tests: sharing preservation (two refs to same ThunkId → same Rc), lazy thunks stay unevaluated after migration, letrec cycles survive migration, Failed thunks preserve error (`src/arena.rs`)
- [ ] Multi-document corpus tests: `---`-separated pipeline with lazy values crossing boundaries (`tests/corpus/eval/`)

