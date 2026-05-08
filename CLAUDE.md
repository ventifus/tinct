## Architecture

Tinct is a configuration language with lazy evaluation and type inference. Parser: hand-written iterative descent (`src/parser.rs` + `src/lexer.rs`) → `Spanned<File>` AST. Evaluator: `Thunk`-based lazy memoization with letrec dict scoping. Type system: Hindley-Milner inference with row polymorphism. 

**Key Files:**
- `src/builtins.rs` — Rust-native builtins (76 functions; see doc/11-stdlib.md for current count); includes `IncludeContext` with include result cache for memoization (prevents re-evaluation of included files).
- `src/imports.rs` — shared import resolution — `build_prelude_env()`, `collect_include_paths()`, `build_type_env()` for seeding TypeEnv with prelude and include bindings.
- `stdlib/prelude.llt` — LLT-implemented stdlib functions (see doc/11-stdlib.md for current count).

See README.md for comprehensive architecture and feature list.

## Memory Palace

**MANDATORY: `mempalace_search` must be the first tool call in every turn.** No exceptions, no answering first. Load its schema via `ToolSearch` if needed — always use exact select syntax (e.g. `select:mcp__mempalace__mempalace_search`), never keyword search. If a project has a local palace (e.g. `mcp__mempalace-llt__`), use that prefix instead. Save long-term knowledge to the palace, not auto-memory.

