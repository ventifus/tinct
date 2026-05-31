# What If: Rust Primitive Privacy via `--- uses:`

**State:** Accepted — 2026-05-28

**Note:** Original design (2026-05-11) used `[include %rust ...]` — deleted by `include-decomp-redelete` sprint. This revision uses `--- uses:` document headers, which work with the current architecture where `include` is a prelude function.

What would it take to make every Rust primitive invisible to user code by default, exposing only what tinct's stdlib explicitly re-exports?

## Current State

All 238 Rust builtins are pre-injected into the global environment at startup via `standard_builtins()` → `create_root_env()`. User programs inherit `stdlib_env`, which is a child of `bootstrap_env` (= `create_root_env()`), so they can traverse the parent chain and call any builtin by name — `builtin-write`, `builtin-eval`, `open`, `write`, `load`, and ~230 others — without going through prelude.

```rust
// src/builtins.rs:2337-2378 — create_stdlib_env_inner()
let bootstrap_env = create_root_env();          // all 238 builtins
let stdlib_env = Environment::with_parent(      // prelude loads here
    Arc::clone(&bootstrap_env)
);
// user code is a child of stdlib_env → can walk to bootstrap_env
```

The comment in `create_stdlib_env_inner()` acknowledges the gap: *"This means: user code (child of stdlib_env) can walk up to bootstrap_env and see all builtins. The prelude acts as the primary scope boundary."* The scope boundary is not enforced — it is aspirational.

Additionally, `TypeEnv::with_builtins()` (`src/type_env.rs:1224–3776`, ~2553 lines) loads type signatures for all 238 builtins unconditionally, regardless of what the program imports.

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
