# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger






## Arena Allocation: Arena-Based Thunks and Flat Environments

Replace `Rc<Thunk>` / `Rc<RefCell<Environment>>` with arena-allocated thunks and flat environments for O(1) variable lookup, zero Rc overhead, and section-scoped bulk deallocation. See doc/08-evaluation.md §Allocation Strategy and doc/whatif/arena-patterns.md.

Design decision (DONE.md): ship Phase 1+2 together as a single migration — variable resolution pass + ThunkArena + EnvArena. Starting with Dict alone creates a hybrid model requiring a second migration.





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

