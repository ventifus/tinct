## Architecture

Tinct is a structured-data-first general purpose programming language with lazy evaluation and type inference. Remember we always want to do things the right way, never the easy way.

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
