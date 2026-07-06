## Architecture

Tinct is a structured-data-first general purpose programming language with lazy evaluation and type inference. 

Axioms:
- **Prelude speaks the Rust protocol**: Rust defines the protocol; prelude implements it. Rust never embeds prelude-specific behavior. Prelude works because it is correct tinct, not because Rust accommodates it.
- **No fast paths, no fallbacks, no backwards compatibility**: one correct path. Fast paths, fallback branches, and legacy shims create parallel implementations that diverge. Old behavior is replaced, not preserved.
- **Correctness, not performance**: performance is not a design concern. Write the provably correct implementation. Never add complexity to skip a check or avoid an allocation.
- **Loader/prelude agnosticism**: users can replace the loader and prelude with their own stack. Language features must be agnostic to what is in the loader and prelude — a feature that only works with the default prelude is not a language feature.
- **General case, not specific**: we build blocks, not solutions. Solve the general problem; do not implement special cases that happen to work for the current caller.

Documentation:

- README.md
- Read @doc/quickstart.md to learn about writing Tinct code
- General Documentation: doc/*.md
- Feature Documentation: doc/feature/*.md
- Proposals for Future Features: doc/whatif/*.md

## Critical Rules

**MANDATORY: `mempalace_search` must be the first tool call in every turn.** No exceptions, no answering first. Load its schema via `ToolSearch` if needed — always use exact select syntax (e.g. `select:mcp__mempalace__mempalace_search`), never keyword search. If a project has a local palace (e.g. `mcp__mempalace-llt__`), use that prefix instead. Save long-term knowledge to the palace, not auto-memory.

**When Deferring** anything that gets put off for the future must exist as item in tracker! NEVER defer anything without tracking it! If you don't track it you'll forget about it! This also means if you come across ANYTHING that hasn't been done yet, make sure it's in tracker.

Development philosophy: When encountering an issue, investigate and address the root cause. Never add workarounds or special-cases! Never take "the simplest approach", only ever take the correct approach!
