## Architecture

Tinct is a configuration language with lazy evaluation and type inference. Parser: pest PEG grammar → `Spanned<File>` AST. Evaluator: `Thunk`-based lazy memoization with letrec dict scoping. Type system: Hindley-Milner inference with row polymorphism. See README.md for comprehensive architecture and feature list.

## Memory Palace

**MANDATORY: `mempalace_search` must be the first tool call in every turn.** No exceptions, no answering first. Load its schema via `ToolSearch` if needed — always use exact select syntax (e.g. `select:mcp__mempalace__mempalace_search`), never keyword search. If a project has a local palace (e.g. `mcp__mempalace-llt__`), use that prefix instead. Save long-term knowledge to the palace, not auto-memory.

