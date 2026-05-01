# What If: Decoupling the eval↔builtins Circular Dependency

**State:** Proposal

What would it take to break the mutual dependency between `src/eval.rs` and `src/builtins.rs`?

## Current State

`src/eval.rs` and `src/builtins.rs` have a circular dependency:
- `builtins.rs` imports from `eval.rs`: `materialize`, `eval_call`, `invoke_function`, `EvalContext`, `EvalResult`
- `eval.rs` imports from `builtins.rs`: `standard_builtins`, `create_root_env`

This is documented in `doc/16-architecture.md §Cross-module coupling` as "safe because dependency is at function-call level, not module init." Safe in Rust — but prevents independent testing of builtins (they require the full evaluator to link) and makes the architecture harder to understand.

### What's Missing

1. The ability to test builtins without linking the full evaluator
2. A clear interface boundary between "evaluation primitives" and "builtin implementations"

## Design

**Audit import depth first, then choose the boundary.** The circular dependency is currently safe and documented. Breaking it requires understanding what subset of eval.rs builtins actually need.

### Import audit (estimated from architecture)

| builtins.rs needs from eval.rs | Purpose |
|-------------------------------|---------|
| `materialize(thunk, span, ctx, depth)` | Builtins force their arguments |
| `eval_call(...)` | `$try`, `$apply`, `$until` call user functions |
| `invoke_function(...)` | Higher-order builtins call lambda args |
| `EvalContext` | Builtins receive ctx for $include |
| `EvalResult<T>` | Return type |

| eval.rs needs from builtins.rs | Purpose |
|-------------------------------|---------|
| `standard_builtins()` | Registers builtins in root env |
| `create_root_env()` | Initial environment setup |

### Approach A: `src/eval_core.rs` interface module (recommended)

Extract the subset of eval.rs that builtins need into a thin `eval_core.rs`:

```
src/eval_core.rs:
  - EvalContext
  - EvalResult<T>  
  - fn materialize(...)
  - fn invoke_function(...)
  - trait Callable (implemented by Value::Function and Value::Builtin)

src/eval.rs: imports eval_core.rs
src/builtins.rs: imports eval_core.rs (NOT eval.rs)
```

`eval.rs` would import from `builtins.rs` (for `standard_builtins`), but `builtins.rs` would no longer import from `eval.rs` — it imports from `eval_core.rs` which `eval.rs` also imports from. No cycle.

**Cost:** Medium. `EvalContext`, `materialize`, `invoke_function` are non-trivial to extract without pulling in most of eval.rs.

### Approach B: Trait object for eval operations

Define a `trait Evaluator` in a separate crate:

```rust
trait Evaluator {
    fn materialize(&self, thunk: &Rc<Thunk>, depth: usize) -> EvalResult<Value>;
    fn invoke_fn(&self, func: &Value, args: &[Rc<Thunk>]) -> EvalResult<Value>;
}
```

Builtins receive `&dyn Evaluator` instead of calling `materialize()` directly. Higher indirection, harder to inline. Not recommended for the hot path.

### Approach C: Feature-flag isolation (testing only)

Keep the circular dependency but add `#[cfg(test)]` stubs for the eval functions that builtins call. Allows unit-testing builtins with mock evaluation. Lowest effort, doesn't improve production architecture.

### Estimated diff size

A rough estimate: extracting `EvalContext`, `materialize`, and `invoke_function` from `eval.rs` would touch ~300 lines in eval.rs + ~100 lines of new eval_core.rs + ~50 lines of import changes in builtins.rs. Not trivial. **Only worthwhile if independent builtin testing is a concrete need.**

## What Would Change

### New: `src/eval_core.rs`

**Proposed:** ~100 lines extracting `EvalContext`, `EvalResult`, `materialize`, `invoke_function`.
**Impact:** Medium — eval.rs loses ~150 lines, gains an import.

### `src/builtins.rs`

**Proposed:** Import changes from `eval.rs` → `eval_core.rs`.
**Impact:** Minor — only import paths change.

## Phased Adoption

### Phase 1: Audit

Map exactly what builtins.rs imports from eval.rs (grep the actual import list). Confirm the estimated list above is correct. Calculate exact diff size.

### Phase 2: Extract eval_core.rs (if justified)

If Phase 1 confirms the dependency is narrow (~5 items) and independent builtin testing is needed: extract to eval_core.rs.

### Trigger

- Phase 1: immediately (the audit is a grep, not a refactor)
- Phase 2: when builtin tests are needed that can't link the full evaluator, OR when the evaluator is being refactored and decoupling would reduce the blast radius

## References

- `doc/16-architecture.md §Cross-module coupling` — current documented state of the circular dependency
