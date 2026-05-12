# What If: `builtin-*` Privacy for tinct

**State:** Accepted — 2026-05-11

What would it take to restrict `builtin-*` stable aliases to prelude-internal use only, preventing user code and non-prelude stdlib from calling them directly?

## Current State

`builtin-*` names (`builtin-if`, `builtin-lt`, `builtin-eq`, `builtin-add`, `builtin-sub`, `builtin-mul`, `builtin-div`, `builtin-filter`, `builtin-map`, `builtin-reduce`, `builtin-take`, `builtin-drop`) are registered in `create_root_env()` (`src/builtins.rs:1367–1427`) as stable aliases for the corresponding Rust builtin functions. A thirteenth name, `builtin-get`, is registered in `standard_builtins()` as the primitive dict accessor (not an alias).

These aliases exist so that `prelude.llt` can always reach the raw Rust implementation even when the user shadows the public names (`<`, `=`, `+`, etc.). For example, `prelude.llt` defines `<` as a wrapper that provides better error messages — but internally its implementation calls `builtin-lt` directly, so it cannot accidentally recurse into itself if the user redefines `<`.

**The problem:** because `builtin-*` names live in the root environment (the same layer that user code inherits), they are globally visible. Any `.llt` file can call `builtin-lt` directly. Discovered call sites in stdlib (2026-05-09 survey):

- `stdlib/prelude.llt` — extensive use (correct and intentional)
- `stdlib/macros.llt` — uses `builtin-if`, `builtin-lt`, `builtin-add`, `builtin-get`, `builtin-reduce`, `builtin-eq`
- `stdlib/path.llt` — uses `builtin-if`, `builtin-eq`, `builtin-sub`, `builtin-add`, `builtin-get`
- `stdlib/toml-lite.llt` — uses all `builtin-*` names extensively (this module is effectively a second prelude-level module)

User code has also been found using them (`samples/versions.llt`, fixed 2026-05-09).

### What's Missing

1. No enforcement boundary between prelude-internal names and user-visible names.
2. Non-prelude stdlib files (`macros.llt`, `path.llt`, `toml-lite.llt`) bypass the prelude's public API and call Rust implementations directly, which means they are not affected by user-provided wrappers.
3. No type-checker warning when user code (or non-prelude stdlib) references `builtin-*` names.

## Why `builtin-*` Privacy Matters for tinct

The `builtin-*` names are a layering violation waiting to cause bugs. The prelude defines wrappers (`<`, `=`, `+`, etc.) that add error messages, type coercion, or dispatch logic on top of the Rust primitives. When downstream code bypasses these wrappers, it misses those improvements — and it silently breaks if the wrapper's semantics are ever tightened.

Concretely: if a future sprint adds range checking to `+` (e.g., warn on integer overflow beyond 2^53), code calling `builtin-add` directly will never trigger that check. The visibility problem is also an ergonomics problem: `builtin-lt` is meaningless to a user; `<` is not.

## Design

### Approach A: Env-layer isolation (full enforcement)

Remove the `builtin-*` aliases from `create_root_env()` and instead inject them only into the environment used when evaluating `prelude.llt`. The evaluator builds a chain:

```
root_env (standard_builtins only, no builtin-* aliases)
  └── prelude_internal_env (+ builtin-* aliases injected here)
        └── prelude_output_env (only the exported prelude dict is visible)
              └── user env
```

The prelude is evaluated in `prelude_internal_env` so it can see `builtin-lt` etc. The resulting bindings (`<`, `=`, `not`, `if`, ...) are promoted into `prelude_output_env`, which becomes the parent of the user env. `builtin-*` names never reach `prelude_output_env` and are therefore invisible to user code and non-prelude stdlib.

**Impact on macros.llt, path.llt, toml-lite.llt:** These must be migrated before this approach can ship. Each file currently uses `builtin-*` names that would become undefined. Migration means replacing every `builtin-if` with `if`, `builtin-eq` with `=`, etc. — the idiomatic prelude wrappers they were already supposed to be using. This is safe because the prelude wrappers have matching semantics for the cases these files exercise.

`builtin-get` is a special case: the prelude's `get` wrapper adds a KeyNotFound error on missing keys, which is stricter than `builtin-get`'s direct error. The migration for `builtin-get` call sites must confirm that the error-on-missing semantics are acceptable (they are for path.llt and toml-lite.llt, which already handle missing keys).

After migration, the `builtin-*` names are invisible outside the prelude evaluation context. Any user or stdlib file that calls them gets `undefined variable: builtin-lt`, the same error they would get for any other unknown name.

### Approach B: Type-checker warning (soft enforcement)

Keep the names globally visible but emit a `T-code` type-checker warning when user code (or non-prelude stdlib) references any name matching `^builtin-`. The type checker knows the source file being checked — it can suppress the warning for `stdlib/prelude.llt` and emit it for everything else.

This requires no runtime change and no migration of `macros.llt`, `path.llt`, or `toml-lite.llt` — those files would produce warnings until they are migrated. The warning is suppressable per-file with a pragma if needed.

**Limitation:** warnings are not errors unless `--strict` is passed. User code that ignores warnings can still call `builtin-lt` indefinitely. This is a nudge, not a hard boundary.

### Approach C: Rename to `__builtin-*` (discouragement only)

Rename all aliases from `builtin-*` to `__builtin-*` (double-underscore prefix, a conventional "implementation detail" signal in many languages). The names remain globally visible but are less guessable and visually ugly enough to deter casual use.

**Limitation:** this is purely a naming convention, not enforcement. A determined user can still call `__builtin-lt`. It also requires migrating `prelude.llt` and all three non-prelude stdlib files to the new names.

### Recommended Approach: A + B in sequence

1. **First: migrate non-prelude stdlib files** — replace `builtin-*` calls in `macros.llt`, `path.llt`, and `toml-lite.llt` with their idiomatic equivalents. This is the right fix regardless of which enforcement approach is chosen.

2. **Then: Approach A** — inject `builtin-*` aliases into the prelude evaluation layer only. After the stdlib migration, there are no remaining legitimate call sites outside `prelude.llt`, so the env-layer isolation becomes safe to apply.

3. **Approach B as a cross-check** — add the type-checker warning as a secondary guard. If a future stdlib module is added and accidentally uses `builtin-*`, the warning catches it at `--strict` time even before the runtime would error.

Approach A is the correct long-term design. Approach B is cheap and provides defense-in-depth.

## What Would Change

### Evaluator / Environment Construction

**Current:** `create_root_env()` registers `builtin-*` aliases in the same environment layer as `standard_builtins()`.

**Proposed:** Split into `create_root_env()` (no aliases) and `create_prelude_eval_env()` (root + aliases). The prelude evaluator uses `create_prelude_eval_env()`; the user evaluator uses the output of prelude evaluation (which does not expose aliases).

**Impact:** Moderate. Requires threading a distinct environment through the prelude loading path in `src/builtins.rs` and `src/imports.rs`.

### Type Checker

**Current:** No warning for `builtin-*` name references.

**Proposed:** In `typecheck.rs` name-resolution, check if the resolved name matches `^builtin-` and the source file is not `prelude.llt`; if so, emit a `T-code` warning (new code, e.g., `T009: direct use of internal builtin alias`).

**Impact:** Minor. One pattern match in the name-resolution path.

### stdlib Migration

**Current:** `macros.llt`, `path.llt`, `toml-lite.llt` use `builtin-*` directly.

**Proposed:** Replace every call site with the idiomatic prelude wrapper. Specific replacements:
- `builtin-if` → `if`
- `builtin-eq` → `=`
- `builtin-lt` → `<`
- `builtin-add` → `+`
- `builtin-sub` → `-`
- `builtin-mul` → `*`
- `builtin-get` → `get` (confirm error-on-missing is acceptable)
- `builtin-reduce` → `reduce` (prelude wrapper)
- `builtin-map` → `map`
- `builtin-filter` → `filter`

**Impact:** Minor per-file. Low regression risk — prelude wrappers have identical semantics for the cases these files exercise.

## Prerequisites

- No external prerequisites. This is a self-contained internal refactor.
- The stdlib migration (`macros.llt`, `path.llt`, `toml-lite.llt`) must precede the env-layer isolation (Approach A), but can be done incrementally file by file.

## References

- The escape-hatch alias pattern is used in Haskell's Prelude for similar reasons: `GHC.Base.map` vs user-shadowable `Prelude.map`. The key difference is that Haskell exposes module qualification as the escape hatch, while tinct uses a name prefix.
