# Name Resolution

This document is for Rust contributors working in `src/resolve.rs` and callers that interact with the `ResolutionTable` or `scope_frames` threading. Tinct developers: the key runtime consequence is that every variable reference in your code is bound at parse time to a `(level, slot)` de Bruijn coordinate — there is no dynamic name lookup at eval time, only integer-indexed scope hops.

The name resolution pass walks the Surface AST after desugaring and before typechecking. It assigns de Bruijn `(level, slot)` coordinates to every runtime variable reference (`VarRef` node), writing results into two parallel outputs: inline `Resolution` OnceLocks on each AST node, and a `ResolutionTable` map keyed by node identity.

The resolver lives entirely in `src/resolve.rs`. It has no knowledge of the runtime evaluator or type checker — it only walks AST structure and manages a scope stack.

---

## Pipeline Position

```
Parse → Surface AST
Desugar → Surface AST (mutates: $_ → Fn, Pipe → Call, etc.)
Resolve → Resolution OnceLocks populated + ResolutionTable produced   ← this pass
Typecheck → reads ResolutionTable, writes CallDispatch OnceLocks
Lower → reads Resolution OnceLocks + CallDispatch OnceLocks + scope_frames → CoreExpr
Eval → reads de Bruijn coords from CoreExpr::Var
```

**Invariants:**
1. Must run after desugaring — desugar rewrites `$_` to `Fn` nodes before the resolver sees them. Running before desugar would encounter `VarRef("_")` in positions that are fn params, not variable references.
2. Must run before typechecking and evaluation — both subsystems consume the coordinates. The type checker calls `resolve_surface_program` internally (see §Interaction with Typechecking).
3. Each `Resolution` OnceLock is written exactly once per node instance. `Clone` on `Resolution` resets to empty — cloned nodes exist in new scopes and must be re-resolved.

---

## Outputs

### `Resolution` — inline OnceLock on each VarRef

```rust
pub struct Resolution(std::sync::OnceLock<Option<(u32, u32)>>);
```

Three states:

| `get()` return | Meaning |
|---|---|
| `None` | Not yet resolved (resolver has not run on this node) |
| `Some(Some((level, slot)))` | Resolved: de Bruijn level and slot |
| `Some(None)` | Explicitly unresolvable (e.g., name not found; also used for `leading-dot` name-not-found) |

`Clone` resets to `None` (empty). This is intentional: a cloned `Arc<SurfaceNode>` has a different pointer identity and lives in a potentially different scope — it must be resolved fresh.

`Resolution` is also the type used on `VarRef` nodes that appear in match arm pattern position — these are pin patterns. The semantics are identical: the OnceLock holds the resolved coordinates of the pinned name, or `Some(None)` when the name was not in scope (which causes the arm to silently not match at runtime).

### `ResolutionTable` — map keyed by node identity

```rust
pub type ResolutionTable = std::collections::HashMap<NodeId, (u32, u32)>;
```

A `NodeId` is the raw `Arc` pointer value (`Arc::as_ptr`), used as a stable identity for an `Arc<SurfaceNode>`. The resolver writes to both the inline `Resolution` OnceLock and the `ResolutionTable` simultaneously for every resolved VarRef.

The lowerer reads from the inline OnceLock when it has the `Arc<SurfaceNode>` directly. The `ResolutionTable` is consumed by passes that receive it separately — notably the type checker (`InferState.resolution_table`) for slot-indexed VarRef lookup.

`ResolutionTable` entries are only written for successfully resolved VarRefs. Unresolved nodes have no entry in the table and their OnceLock is left unset (`None`).

---

## De Bruijn Coordinates

Variables are addressed by `(level, slot)`:

- **`level`** — number of scope-stack hops from the innermost scope at the use site to the scope that owns the binding. Level 0 = current (innermost) scope. Level 1 = immediate parent. Computed as the offset of the owning scope from the end of `SurfaceResolver.scopes` during the walk.
- **`slot`** — ordinal position of the name within that scope's `IndexMap<String, u32>`. The slot assigned at resolution time matches the slot that the evaluator will allocate for that binding in the `ScopeArena`.

Example:

```
[outer: 42  inner: [fn [let x] $outer]]
```

When resolving `$outer` inside the fn body, the scope stack (innermost first) is:
```
[fn-params: {x=0}]                      ← level 0
[dict-letrec: {outer=0, inner=1}]       ← level 1
[runtime env (builtins, %)]             ← level 2+
```

`$outer` resolves to `(level=1, slot=0)`.

---

## Resolver Internals

```rust
struct SurfaceResolver {
    scopes: Vec<IndexMap<String, u32>>,   // innermost last; iterated in reverse for lookup
    table: ResolutionTable,
    unresolved: Vec<(String, Span)>,       // VarRefs not found in any scope
    suppress_depth: usize,                 // > 0 = inside non-runtime position
}
```

`resolve_name(name)` iterates `scopes` in reverse (innermost first):

```rust
for (offset, scope) in self.scopes.iter().rev().enumerate() {
    if let Some(&slot) = scope.get(name) {
        return Some((offset as u32, slot));
    }
}
None
```

`offset` is the de Bruijn level: 0 for the innermost scope, 1 for its parent, and so on.

### Scope Frame Format

Each scope frame is an `IndexMap<String, u32>` mapping `name → slot`. The stored `u32` is the actual slot index in the evaluator's `ScopeArena` — it matches the ordinal position in which that name was declared within its scope. For dict scopes (letrec), this is the declaration order of the keys.

Initial frames seeded from outside (capabilities, prelude) are added via `enter_scope_from_frame`, which clones the frame directly. The slot values in these frames are the actual `ScopeArena` slot indices as of when the frame was captured from the root environment.

---

## Scope Construction by Construct

### Dict

```
[a: 1  b: $a  c: [fn [let x] $b]]
```

1. Walk key expressions in the **outer** scope (before entering the dict's letrec scope). Static (non-escaped) VarRef keys like `a:` are declaration positions — `suppress_depth` is incremented so lookup failure is not an error.
2. Collect all static keys: `surface_dict_static_keys(entries)` → `["a", "b", "c"]`.
3. `enter_scope(["a", "b", "c"])` — pushes `{a=0, b=1, c=2}` onto `scopes`. All values share one letrec scope (mutual recursion is supported).
4. Walk all value expressions inside this scope.
5. `exit_scope()`.

### Fn

```
[fn [let x y] body]
```

1. Walk param annotations in the **outer** scope (type names, not runtime references — `suppress_depth` is incremented).
2. `enter_scope(["x", "y"])` — pushes `{x=0, y=1}`.
3. Walk `body` inside this scope.
4. `exit_scope()`.

### Sequential (multi-body fn or document-level expressions)

```
[a: 1]
[b: $a]       ← $a sees the first dict's keys
[c: $b]       ← $c sees both prior dicts' keys
```

Sequential expressions inject their static keys into subsequent expressions. For each non-last expression:

1. Walk the expression.
2. If the result is a Dict with static keys, `enter_scope(keys)` (scope stays open for subsequent expressions).

All injected scopes are exited together at the end. This mirrors the evaluator's sequential scope-chain semantics exactly.

The same logic applies at the document level: `walk_surface_document` tracks injected scopes, collects the new frames at the end (innermost last), and returns them as `new_frames`. These frames represent the document's contribution to the scope chain and are passed as `initial_frames` for the next document.

### Match arms

```
[match x
  Color.Red: "red"
  ...:       "other"]
```

For each non-CaseArm match arm, the resolver walks `arm.pattern` with `suppress_depth` incremented by 1. This means:

- All VarRefs in pattern position whose names are not found in scope produce `Some(None)` (OnceLock set to unresolvable) rather than emitting an "undefined variable" error. The eval dispatch in `match_pattern` treats `Some(None)` as "arm does not match" — the arm is silently skipped for this scrutinee.
- VarRefs that ARE in scope produce `Some(Some((level, slot)))` as normal — these become pin patterns (equality check against the in-scope value).
- No scope is created for the arm pattern itself. The arm body and guard see the same scope as the scrutinee.

### CaseArm `[case [let n m] pattern body]`

1. Walk `let_bindings` with `suppress_depth += 1` (names in `[let n m]` are declarations).
2. Extract the declared names from the `LetDecl` bindings.
3. `enter_scope(declared_names)`.
4. Walk `pattern` and `body` inside the scope.
5. `exit_scope()`.

The `enter_scope` happens before walking the pattern, so that pattern VarRefs (e.g., `v` inside `[Result.Ok v]`) resolve into the case arm's own scope rather than leaving `OnceLock`s unset.

### Anonymous instance entries

The lowerer flattens anonymous `InstanceDecl` entries into the enclosing dict under mangled names like `ɪɴꜱᴛᴀɴᴄᴇ⧼Comparable∷<⟨Int⟩⧽`. `surface_dict_static_keys` replicates this mangling when collecting dict keys, so method implementations within the same dict (e.g., `>` calling `<` from the same instance) can resolve each other via the letrec scope.

---

## suppress_depth

`suppress_depth > 0` suppresses unresolved-VarRef error recording. When a VarRef is not found in any scope and `suppress_depth > 0`, the lookup silently returns `None` (OnceLock left unset) without adding to `unresolved`. This prevents false-positive errors for positions that are not runtime variable references.

Positions that increment `suppress_depth`:

| Position | Reason |
|---|---|
| Annotation contents (`@Int`, `@[type: T]`) | Type names, not runtime refs |
| Non-escaped dict keys (`x:`) | Name declarations, not references |
| `LetDecl`/`PatternDecl` binding names (`[let x]`) | Name declarations |
| Instance pattern arms (`[let a@String]`) | Type-matching context |
| Instance method-name keys (in implementation) | Declaration position |
| Non-CaseArm match arm patterns (`arm.pattern`) | Pattern VarRefs are pins or wildcards, not runtime lookups; unresolvable names produce `Some(None)` → arm silently does not match |

Escaped dict keys (`$x:`) and annotation PropertyDict values (in constructor bodies) are intentionally **not** suppressed — they are runtime expressions and must resolve correctly.

---

## Special Cases

### `method_to_instance` — class method fallback

When `resolve_name` fails for a name that looks like a class method (e.g., `+`, `=`), the resolver falls back to scanning all scopes for an instance binding whose method component matches. Instance binding names have the format `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽` or `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⧽`. The resolver resolves to the first match.

The type checker overrides this with the specific instance via `CallDispatch` (a separate `OnceLock` on the same `VarRef` node) when it can determine the correct instance. The resolver's best-effort fallback ensures the OnceLock is set and the lowerer doesn't emit "undefined variable" for method names.

### Leading-dot Field (`.name`)

```
.name   →   SurfaceExpression::Field { expr: None, field: DotKey::Ident(name), resolution, .. }
```

Resolves `name` in the current scope to get the de Bruijn **level** (slot is discarded). The lowerer uses the level with hardcoded root slot constants (`FIELD_GET_ROOT_SLOT`, `SLOT_GET_ROOT_SLOT`) because leading-dot always resolves to a builtin in the root env. If `field-get` is not in scope (resolver not seeded with the runtime env), the `OnceLock` is left unset and the lowerer emits a `LowerDiagnostic::Error`.

### `expr.field` Field access

Resolves `field-get` in scope to get the level (slot discarded). Same purpose: establishes the de Bruijn level for the root builtin lookup. If not found, the OnceLock is left unset and lowering fails.

Both `.name` and `expr.field` set `Some(None)` on `Field.resolution` when the name was not found, distinguishing "resolver ran but name not in scope" from "resolver did not run" (`None`).

### Quote

Variables inside a `Quote` node are AST data, not runtime references. The resolver does not walk into `Quote` at all.

### TypeAlias body

Constructor names, field type expressions, and type parameters are type-level — walking them would produce false "undefined variable" errors. The resolver surgically walks **only** annotation PropertyDict values on constructors, because those are runtime closures stored via `builtin-make-annotated`.

### VarRef in pattern position (pin)

A `VarRef` node inside a match arm pattern (or inside a CaseArm's structural pattern) is a pin. The resolver writes the de Bruijn coordinates of the name as found in the scope active at the pattern site. `Some(None)` means the name is not in scope; at runtime the evaluator silently skips the arm (no-match). Named bindings in match arms always require `[case [let n] ...]` — a bare VarRef in pattern position is never a fresh binding.

---

## Entry Points

```rust
pub fn resolve_surface_program(
    program: &SurfaceProgram,
    initial_frames: &[IndexMap<String, u32>],
) -> (ResolutionTable, Vec<IndexMap<String, u32>>)
```

Resolves all documents in a `SurfaceProgram`. `initial_frames` are scope frames from prior resolver runs (e.g., the root capability frame) seeded outermost-first. Returns the `ResolutionTable` and `new_frames` — the frames added by this program's documents, which can be passed as `initial_frames` for a subsequent program.

The resolver processes documents sequentially: each document's new_frames accumulate in order. Frames from document N are visible to document N+1 and beyond.

```rust
pub fn resolve_surface_document_inplace(
    doc: &SurfaceDocument,
    initial_frames: &[IndexMap<String, u32>],
) -> (ResolutionTable, Vec<(String, Span)>, Vec<(String, Span)>, Vec<IndexMap<String, u32>>)
```

Resolves a single document. Also returns:
- `errors` — `(name, span)` pairs for genuinely unresolved expression-position VarRefs.
- `warnings` — `(message, span)` pairs for lost intermediate bindings and unused function parameters.

Used by `builtin-resolve` in the meta API for per-document incremental resolution. The `builtin-resolve` function returns errors in `diagnostics` (preserving the existing error-only contract) and warnings in a separate `warnings` key.

Both functions are purely functional with respect to the AST (no mutation visible to callers), but they do write to the inline `Resolution` OnceLocks on each VarRef node.

---

## Interaction with Other Subsystems

### The Loader Pipeline (`src/lib.rs::run_loader_pipeline`)

The production path resolves the init program (loader.llt) once, seeded with a root frame built from `ScopeArena.scopes[0]`:

```
root_frame = arena.scopes[0].iter_named() → IndexMap<String, u32>
(table, new_frames) = resolve_surface_program(&loader_program, &[root_frame])
all_frames = [root_frame] + new_frames
eval_ctx_with_frames = eval_ctx.with_scope_frames(Arc::new(all_frames))
```

The combined `all_frames` is attached to the `EvalContext` via `with_scope_frames()`. This makes the frames available to `lower()` at thunk-forcing time (via `thunk_ctx.scope_frames` / `ctx.scope_frames`), which uses them to resolve `CallDispatch` mangled instance binding names to de Bruijn coordinates (B-513).

The root frame is the **one legitimate exception** to purely AST-based scope construction: capabilities injected by `main.rs` (`%cwd`, `%libdir`, `%programs`, etc.) have no AST source, so their names must be read from `ScopeArena.scopes[0]`. After this single read, the resolver operates purely from AST structure.

### `EvalContext.scope_frames`

```rust
pub scope_frames: Option<Arc<Vec<IndexMap<String, u32>>>>,
```

Set by `with_scope_frames()` after resolving the init program. `None` in bootstrap contexts and unit tests. Propagated unchanged to all child contexts (`with_base_dir`, `with_cancel_token`, timeout contexts, etc.).

`scope_frames` is read by `lower()` in `eval_materialize.rs` when forcing an unevaluated thunk that holds a `SurfaceNode`. The frames are passed to `lower()` so it can resolve `call_dispatch`-annotated VarRefs by name lookup when forcing thunks at eval time.

### Lowering (`src/lower.rs`)

`lower(arc, scope_frames)` is called at two points:
1. **At eval time** (`eval_materialize.rs`): when an `Unevaluated` thunk holding a `SurfaceNode` is forced. `scope_frames` comes from `thunk_ctx.scope_frames` or `ctx.scope_frames`.
2. **During `eval_surface_file`** (`eval.rs`): when processing the top-level document expressions before allocating thunks.

The lowerer reads `VarRef.resolution` (set by the resolver) to emit `CoreExpr::Var { level, slot }`. If the OnceLock is unset (`None`), it emits `LowerDiagnostic::Error` — "resolver did not run".

For `CallDispatch`-annotated VarRefs (typeclass method calls resolved by the type checker), the lowerer uses `scope_frames` to look up the mangled instance binding name by calling `resolve_name_in_frames`. If `scope_frames` is `None`, or the name is not found, it falls back to the `Resolution` OnceLock (the resolver's best-effort `method_to_instance` result). If neither is set, it emits `LowerDiagnostic::Error`.

### Interaction with Typechecking (`src/typecheck.rs`)

The type checker calls `resolve_surface_program` internally to build its own `ResolutionTable` (stored in `InferState.resolution_table`). This is separate from the lowerer's use of inline OnceLocks. Two paths exist:

- **With `eval_ctx`** (`typecheck_surface_program_annotation_table_with_env`): seeds the resolver from `ScopeArena.scopes[0]` to match the production loader path, so instance binding names resolve correctly.
- **Bootstrap** (`typecheck_surface_program_annotation_table` called from `typecheck_source`): passes empty `initial_frames` (`&[]`).

The type checker also writes `CallDispatch` OnceLocks on call-site VarRef nodes when it can determine the concrete instance for a typeclass method call. This rewrite happens during type-checking, after the resolver has already run; the lowerer reads `CallDispatch` at a later point.

### The `builtin-resolve` meta API (`src/builtins_meta.rs`)

`builtin-resolve` is the tinct-code-facing counterpart of `resolve_surface_document_inplace`. It takes a `Value::Document` and a frames dict (integer-keyed outer dict, string→int inner dict) and resolves the document in place. The frames dict is the tinct-level representation of `initial_frames`.

Key behaviors:
- Only accepts `Value::Document`, not `Value::Program` — one document at a time.
- Returns `{doc: Document, errors: Dict<Int, ErrorDict>}`. **Does not return `new_frames`** — callers use `builtin-scopes` / scope-to-frames introspection to rebuild frames from the `ScopeArena` chain after `builtin-eval` has processed the document.
- `_new_frames` from `resolve_surface_document_inplace` is intentionally discarded in `builtin-resolve`. The rationale: `builtin-eval` writes bindings into the `ScopeArena`; tinct callers reconstruct frames from the resulting scope chain rather than from the resolver's output.

### Macro expansion (`src/builtins_meta.rs`)

When a macro is expanded at runtime, the macro body is a fresh `SurfaceProgram`. The expander seeds the resolver from the full parent chain of the call-site `FlatEnv` via `arena.collect_parent_chain(call_site_env_id)`, converting each scope's `iter_named()` entries into an `IndexMap<String, u32>`. This is correct for macros: they are generated at runtime and must see the same lexical scope as the call site, which is reflected by the runtime `ScopeArena` chain rather than the static AST-derived frames.

---

## Capability Name Injection

Capabilities like `%`, `%cwd`, `%libdir`, `%stdin` are injected by `main.rs` into `ScopeArena.scopes[0]` before any user code runs. Their names appear in the root scope frame, which is the only frame captured directly from the `ScopeArena`. All other names are derived purely from AST structure.

The root frame filter excludes empty names and `#`-prefixed names (internal synthetic slots):

```rust
arena.scopes[0]
    .iter_named()
    .filter(|(n, _)| !n.is_empty() && !n.starts_with('#'))
    .map(|(n, slot)| (n.to_string(), slot))
    .collect()
```

Without this root frame, `%`, `%cwd`, and builtin names would not be found in any resolver scope, and all references to them in the program text would be reported as unresolved.

---

## Two-Path Architecture (Historical Context)

Before S-925, the resolver used two sources to build its initial scope stack:
1. **Seeding from `FlatEnv.slot_names`** (the evaluator's runtime state)
2. **AST walking** via `walk_surface_document` / `enter_scope`

These diverged whenever a new scope-creating construct was added — updating only one source produced wrong de Bruijn coordinates and "resolver level N out of range" errors at runtime.

S-925 consolidated to a single path: the resolver receives `initial_frames` as a plain `Vec<IndexMap<String, u32>>` (no `FlatEnv` reading), AST-walks everything else, and returns `new_frames` so callers can thread them forward. The single remaining `FlatEnv` read (the root capability frame) is the documented exception.

The old fallback seeding path via `collect_parent_chain` was removed by T-1585.

---

## Qualified Names (`TypeName.CtorName`)

Qualified names like `Color.Red` are **not resolved by the resolver**. They are parsed as `SurfaceExpression::Field { expr: Some(VarRef("Color")), field: DotKey::Ident("Red") }`. At resolution time, only `Color` (the `VarRef` sub-expression) is resolved to a de Bruijn coordinate. `Red` is a string literal field key — no coordinate is assigned to it.

The field access is lowered to a `field-get` call: `[field-get "Red" Color]`. The evaluator then accesses the `Red` key from whatever value `Color` resolves to at runtime (typically a dict of constructors). There is no compile-time check that `Color` is a type dict or that `Red` is a valid constructor name — that is the type checker's job.

In pattern position, `Color.Red:` is syntactic — the parser assembles the constructor tag string `"Color.Red"` directly without a runtime dict lookup. No resolver coordinates are involved.
