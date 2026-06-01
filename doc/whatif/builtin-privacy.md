# What If: Rust Primitive Privacy via `--- uses:`

**State:** Accepted — 2026-05-28

**Note:** Original design (2026-05-11) used `[include %rust ...]` — deleted by `include-decomp-redelete` sprint. This revision uses `--- uses:` document headers, which work with the current architecture where `include` is a prelude function.

What would it take to make every Rust primitive invisible to user code by default, exposing only what tinct's stdlib explicitly re-exports?

## Current State

`standard_builtins()` and `create_root_env()` were deleted in S-784 as part of the modular builtin registry refactor. The current bootstrap path is `create_stdlib_env_inner()` in `src/builtins.rs`.

The remaining gap is at `src/builtins.rs:1523-1528`, where `create_stdlib_env_inner()` injects the full `core_builtins()` list directly into `stdlib_env` after loading the prelude:

```rust
// src/builtins.rs:1523-1528 — create_stdlib_env_inner()
for def in core_builtins {
    let name = def.name.to_string();
    let builtin_val = Value::Builtin(def);
    let thunk = Arc::new(Thunk::new_materialized(builtin_val, Span::origin()));
    stdlib_env.write().unwrap().insert(name, thunk);
}
```

This means all core builtins (including `builtin-eval`, `builtin-write`, `builtin-load`, etc.) are visible to user code as top-level bindings in `stdlib_env`. User programs can call them by name without going through the prelude. Enforcement is deferred to S-785.

Additionally, `TypeEnv::with_builtins()` loads type signatures for all builtins unconditionally, regardless of what the program imports.

### What's Missing

1. No mechanism for stdlib files to declare their Rust dependencies explicitly
2. No enforcement that user code cannot call builtins directly
3. Type checker approves programs using `builtin-write` even without any import
4. ~50 builtins still registered under bare names without `builtin-*` prefix, including all builder ops (`make-builder`, `builder-set`, etc.) and I/O ops (`write`, `open`, etc.) (B-168)

## Design

### The Core Insight: Tinct's Own Scoping Already Provides the Isolation

Tinct's two-dict pattern already demonstrates how to hide implementation details:

```tinct
[
  builtin-if: [...]   # local scope — not exported
]
[
  if: builtin-if      # exported — only this name is visible to includers
]
```

A program that includes this gets the second dict. `builtin-if` is not in the exported value and is unreachable from the caller. No special isolation machinery is needed.

`--- uses:` works the same way. `eval-program` collects the named modules (via `builtin-module`) into a scope dict and passes it to `eval`, which seeds a fresh document-local env frame — like the first dict above. The document's exported dict contains only what it explicitly names. The `builtin-*` names used internally never appear in the exported dict and are therefore unreachable by user code.
