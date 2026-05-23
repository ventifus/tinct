## Architecture

Tinct is a structured-data-first general purpose programming language with lazy evaluation and type inference.

Documentation:

- README.md
- General Documentation: doc/*.md
- Feature Documentation: doc/feature/*.md
- Proposals for Future Features: doc/whatif/*.md

## General Rules

**MANDATORY: `mempalace_search` must be the first tool call in every turn.** No exceptions, no answering first. Load its schema via `ToolSearch` if needed — always use exact select syntax (e.g. `select:mcp__mempalace__mempalace_search`), never keyword search. If a project has a local palace (e.g. `mcp__mempalace-llt__`), use that prefix instead. Save long-term knowledge to the palace, not auto-memory.

**When Deferring** anything that gets put off for the future must exist as a sprint or item in TODO.md or in doc/whatif/ for larger efforts. NEVER defer anything without tracking it! If you don't track it you'll forget about it! If you encounter a "pre-existing" issue, make sure it's in TODO.md!

This also means if you come across ANYTHING that hasn't been done yet, make sure it's in TODO.mc. No exceptions!