# What If: Types as First-Class Runtime Values

**State:** Proposal

What would it take to make type declarations exist as first-class runtime values in tinct, enabling loader to selectively compose a `fundamental-tc` from direct value references like `[builtin-tc-add-type tc NetCap]`?

## Goals

1. **Composable fundamental-tc.** Loader builds its seed TypeContext by explicitly naming the types it provides — `DirCap`, `NetCap`, `Int`, `Bytes`, etc. — as direct value references, not strings and not an all-or-nothing dump of everything Rust knows.

2. **Type values are ordinary values.** A `[type DirCap]` declaration produces a runtime value just like any other dict entry. Loader already has `DirCap` in scope as a value — it should be able to pass that value directly to TypeContext operations.

3. **No privileged Rust channels.** Every type that downstream programs can reference must arrive through the same tinct path as every other value. No implicit Rust fallback, no silent injection, no string-based lookup tables.

## Current State

Types in tinct exist in two separate worlds that do not communicate at runtime.

**The type-checker world:** `state.tycon_env` is a `HashMap<String, TyConDef>` populated when `typecheck_surface_program_with_env` processes `[type ...]` declarations. `TypeContext` carries `tycon_env` as an opaque handle. Programs that want to reuse a TypeContext thread it explicitly through `builtin-typecheck-doc`. The type-checker resolves `@DirCap` by looking up `"DirCap"` in `state.tycon_env`.

**The runtime world:** evaluating `[type DirCap]` produces a `Value::Dict` containing one entry: `"DirCap" → Value::Variant("DirCap.DirCap", {})`. The value `DirCap` in loader's scope is this constructor dict. The runtime knows nothing about what `@DirCap` means.

These worlds meet only inside `builtin-typecheck-doc`: the Rust implementation reads the current TypeContext, seeds the type-checker state from it, and runs inference. There is no mechanism for moving in the other direction — from a runtime value back into a TypeContext.

### The fundamental-tc problem

Loader needs a seed TypeContext (`fundamental-tc`) to pass to `uses-scope` and to the main `builtin-typecheck-doc` call. This TypeContext should contain *only* type declarations — `Int`, `Float`, `Bytes`, `String`, `DirCap`, `NetCap`, `ClockCap`, `Handle`, `Url`, `BuilderHandle`, `Task`, `Channel`, etc. — and none of the function type schemes (`builtin-get`, `builtin-parse`, etc.) that come from evaluating `builtin_core.llt`.

Today, `[builtin-get-type-context]` returns whatever TypeContext Rust initialized — which includes all function schemes. There is no way to say "give me just the type declarations, not the functions."

The obvious fix — letting loader construct `fundamental-tc` by passing its own in-scope type values — requires a bridge from runtime values to type declarations. That bridge does not exist today.

### What already works

`typenode_value_to_type` (in `src/typecheck_annot.rs`) was extended in commit `db87387e` to recognize constructor dicts:

```rust
Value::Dict(entries) if !entries.is_empty() => {
    // All values are Variants with a common qualified prefix?
    // e.g., { "True": Variant("Boolean.True"), "False": Variant("Boolean.False") }
    // → Type::TyCon("Boolean")
    let prefix = /* extract common prefix from variant tags */;
    prefix.map(|p| Type::TyCon(p))
}
```

So `typenode_value_to_type` can already turn a constructor dict into a `Type::TyCon(name)`. The machinery for going from value → type name exists. What is missing is going from value → TypeContext insertion.

### What's Missing

1. A builtin that accepts a type value and inserts its TyConDef into a TypeContext.
2. A pre-populated `ctx.type_context.tycon_env` in main.rs that contains correct TyConDefs for core primitive types (not synthesized on demand, but computed once from running the type-checker over `builtin_core.llt`).
3. A way for loader to selectively pick which types from its own scope to include in `fundamental-tc` without passing all of them.

## Why This Matters for Tinct

- **Composable TypeContext construction.** Loader explicitly lists the types it provides downstream programs:
  ```tinct
  fundamental-tc: [reduce builtin-tc-add-type [builtin-make-type-ctx]
                   [DirCap NetCap ClockCap Handle Int Float Bytes String]]
  ```
  A loader that does not provide `NetCap` simply omits it. Programs that reference `@NetCap` without the appropriate loader get a type error, not silent `Any`.

- **Removes all-or-nothing semantics.** Today loader either gets the full TypeContext (everything including function schemes) or an empty one. Selective injection is impossible.

- **No strings.** `[builtin-tc-add-type tc DirCap]` passes `DirCap` the value directly — the same value that appears in loader's runtime scope. No stringly-typed `"DirCap"` argument that silently succeeds even if the spelling is wrong.

- **Type declarations become self-describing.** A value produced by `[type DirCap]` carries enough information to reconstruct its TyConDef. There is no separate registration step.

## Design

### `builtin-tc-add-type`

A new builtin with signature:

```tinct
builtin-tc-add-type: [fn@TypeContext [let tc@TypeContext val@Any] ...]
```

Behavior:
1. Materialize `val` to a concrete `Value`.
2. Use `typenode_value_to_type` to extract the type name: `Value::Dict { "DirCap": Variant("DirCap.DirCap") }` → `Type::TyCon("DirCap")` → name `"DirCap"`.
3. Look up `"DirCap"` in `ctx.type_context.tycon_env` (the pre-populated master tycon env, described below).
4. Insert the `TyConDef` into `tc`'s `tycon_env`.
5. Return `tc`.

This is a pure lookup — the TyConDef is not synthesized from the runtime value. The runtime value is only used to identify *which* TyConDef to copy from the master registry.

### Pre-populated master tycon registry

`main.rs` initializes `ctx.type_context` with a `tycon_env` pre-computed by running the type-checker over `builtin_core.llt` exactly once at startup. This produces correct `TyConDef` entries for every core type:

- Opaque types (`DirCap`, `NetCap`, `Int`, `Bytes`, etc.) → `TyConDef` with kind `*`, body `Type::TyCon(name)` (opaque/abstract)
- Parameterized types (`Task[a]`, `Channel[a]`, `ReactiveCell[a]`) → `TyConDef` with kind `* → *`

This master registry is read-only after initialization. `builtin-tc-add-type` copies TyConDefs *out of* it into the user-supplied TypeContext — it never modifies the master.

`[builtin-make-type-ctx]` returns an empty TypeContext (empty `inference_env`, empty `tycon_env`). The master registry is not automatically included. Loader builds `fundamental-tc` by selectively adding what it needs.

### `[builtin-get-type-context]` role

After this change, `[builtin-get-type-context]` returns the master TypeContext — the one with all core TyConDefs *and* all function type schemes (the full result of bootstrapping). It remains available for use cases that want everything: the LSP, sandboxed analysis, or a loader that explicitly opts into the full core environment. It is no longer the default for `fundamental-tc`.

### Loader usage

```tinct
[
  # Build a TypeContext containing only the types loader provides.
  # DirCap, NetCap etc. are values in scope from builtin_core.llt evaluation.
  fundamental-tc: [reduce builtin-tc-add-type [builtin-make-type-ctx]
                   [DirCap NetCap ClockCap Handle Url
                    Int Float Bytes String
                    BuilderHandle Task Channel Context ReactiveCell
                    Timezone Uri Urn Decimal BigInt]]

  # uses-scope type-checks modules using this seed
  uses-scope: [fn [let module-names]
    [reduce
      [fn [let acc name]
        [typed: [builtin-typecheck-doc parse-result.program fundamental-tc]]
        ...]
      []
      module-names]]
]
```

The list is explicit. If loader does not include `QuicSession`, programs that declare `--- uses: ["async"]` and reference `@QuicSession` get an `undefined type` error rather than a silent `Any`. This is correct — the error should appear at the point where the program uses an undeclared type, not silently disappear.

### Alternative: type-list shorthand

If the reduce form is verbose, a multi-arg variant works:

```tinct
builtin-tc-add-types: [fn@TypeContext [let tc@TypeContext ...vals] ...]
```

Then:
```tinct
fundamental-tc: [builtin-tc-add-types [builtin-make-type-ctx]
                  DirCap NetCap ClockCap Handle Url Int Float Bytes String ...]
```

Both forms are viable. The variadic form is syntactic sugar over `reduce`.

## What Would Change

### `src/builtins_meta.rs` (or `src/builtins_core.rs`)

**Current:** No builtin for adding a type value to a TypeContext.
**Proposed:** `builtin-tc-add-type` registered in `core_builtins()` (not `meta_builtins()` — loader needs it at bootstrap time, before meta is available). Arity 2: `(tc, val)`. Uses `typenode_value_to_type` to resolve the type name, looks up TyConDef in master registry, inserts into `tc`.
**Impact:** Minor — new builtin, no existing behavior changes.

### `src/imports.rs` — `get_builtin_core_type_env`

**Current:** Returns a `TypeEnv` with function type schemes and TyConDefs mixed together.
**Proposed:** The initialization path separates these: `tycon_env` is populated from `builtin_core.llt` type-checker output; `inference_env` is empty. `get_builtin_core_type_env` continues to return the full set for `[builtin-get-type-context]`.
**Impact:** Moderate — restructures initialization order; master tycon registry must be computed before any TypeContext is created.

### `src/eval.rs` — `TypeContextData`

**Current:** `tycon_env` is populated during `builtin-typecheck-doc` calls and accumulates across calls via the sync mechanism.
**Proposed:** `tycon_env` in a freshly made TypeContext (`[builtin-make-type-ctx]`) starts empty. TyConDefs enter only via explicit `builtin-tc-add-type` or via `builtin-typecheck` processing a `[type ...]` declaration.
**Impact:** Minor — semantics clarified, not changed.

### `stdlib/loader.llt`

**Current:** `fundamental-tc: [builtin-get-type-context]` — receives the full TypeContext including all function schemes.
**Proposed:** `fundamental-tc` is built via `[reduce builtin-tc-add-type ...]` with an explicit list of type values from loader's scope.
**Impact:** Minor — better intentionality, no runtime difference for programs that use only the listed types.

### `stdlib/builtin_core.llt`

**Current:** Declares all core builtins including types. No declaration for `builtin-tc-add-type`.
**Proposed:** Add `builtin-tc-add-type: [fn@TypeContext [let tc@TypeContext val@Any] ...]`.
**Impact:** Trivial — one declaration line.

## Prerequisites

- The `typenode_value_to_type` extension for constructor dicts (commit `db87387e`) — already done.
- A pre-populated master `tycon_env` in `ctx.type_context` at main.rs initialization time. This requires that `get_builtin_core_type_env` separates TyConDefs from function schemes in its output structure, or that a separate `get_builtin_core_tycon_env` function extracts just the TyConDefs.
- `build_core_env()` in `src/imports.rs` must inject the core type values (the constructor dicts for `DirCap`, `NetCap`, etc.) into the initial runtime environment so they are available as names in loader's scope. Without this, `DirCap` is not a value loader can reference — it only exists in the type-checker.

## Connection to Type-Stage Env

The type-stage env already contains constructor dicts. When the type-checker processes `builtin_core.llt`, it evaluates `[type DirCap]` in the type-stage pass and stores the result in `state.type_stage_env`. `resolve_type_name("DirCap")` already finds this value via `get_value_by_name` and calls `typenode_value_to_type` to convert it.

`builtin-tc-add-type` is a companion to this existing mechanism. Where the type-stage env lookup converts type-stage values to types *during type-checking*, `builtin-tc-add-type` converts runtime values to TypeContext entries *before type-checking starts*, enabling loader to pre-seed the TypeContext that subsequent type-check calls will inherit.

The two mechanisms are complementary: the type-stage env handles types that programs declare inline; `fundamental-tc` handles types that come from loader's bootstrap environment.

## References

- `src/typecheck_annot.rs` — `typenode_value_to_type`, extended in commit `db87387e`
- `src/eval.rs` — `TypeContextData` struct with `tycon_env` and `type_stage_env`
- `stdlib/loader.llt` — current `fundamental-tc` construction via `[builtin-get-type-context]`
- `doc/whatif/type-stage-foundation.md` — replacing Rust type-handling with type-stage tinct programs (broader vision)
- `doc/05-type-annotations.md` — type annotation syntax and `@Type` resolution
