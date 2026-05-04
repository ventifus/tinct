# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (Type Predicates, String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## Cycle Findings — C121

### cycle-findings-c121-b: Remaining C121 Items

- [ ] **Minor — resource_limit_exceeded.llt-eval is a placeholder** (`tests/corpus/eval/errors/resource_limit_exceeded.llt-eval`): Expected output is `Dict({"large": Seq(...)})` instead of `[E043]` — test passes without triggering the error. Either implement MAX_COLLECT_SIZE enforcement or move to placeholders/ directory. [test-crafter]
