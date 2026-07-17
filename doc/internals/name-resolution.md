# Name Resolution

The name resolution pass walks the Surface AST after desugaring and before typechecking. It assigns de Bruijn `(level, slot)` coordinates to every runtime variable reference (`VarRef` node), writing results into two parallel outputs: inline `Resolution` locks on each AST node, and a `ResolutionTable` map keyed by node identity.

---

## Pipeline Position

```
Parse → Surface AST
Desugar → Surface AST (mutates: $_ → Fn, Pipe → Call, etc.)
Resolve → Resolution OnceLocks populated + ResolutionTable produced   ← this pass
Typecheck → reads ResolutionTable
Lower → reads Resolution OnceLocks + ResolutionTable → CoreExpr
Eval → reads de Bruijn coords from CoreExpr::Var
```

**Invariants:**
1. Must run after desugaring — desugar rewrites `$_` to `Fn` nodes before the resolver sees them. Running before desugar would encounter `VarRef("_")` in positions that are fn params, not variable references.
2. Must run before typechecking and evaluation — both subsystems consume the coordinates.
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
| `Some(None)` | Explicitly unresolvable (e.g., leading-dot name not found in scope) |

`Clone` resets to `None` (empty). This is intentional: a cloned `Arc<SurfaceNode>` has a different pointer identity and lives in a potentially different scope — it must be resolved fresh.

### `ResolutionTable` — map keyed by node identity

```rust
pub type ResolutionTable = std::collections::HashMap<NodeId, (u32, u32)>;
```

A `NodeId` is the raw `Arc` pointer value (`Arc::as_ptr`), used as a stable identity for an `Arc<SurfaceNode>`. The resolver writes to both the inline `Resolution` OnceLock and the `ResolutionTable` simultaneously for every resolved VarRef.

The lowerer reads from the inline OnceLock when it has the `Arc<SurfaceNode>` directly. The `ResolutionTable` is consumed by passes that receive it separately (e.g., `builtin-eval` in the meta API).

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
[fn-params: {x=0}]           ← level 0
[dict-letrec: {outer=0, inner=1}]   ← level 1
[runtime env (builtins, %)]   ← level 2+
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

### Match arms (Pattern-based)

```
[match x
  [case [let n] SomePattern $n]]
```

`extract_pattern_bindings(pattern)` collects names bound by the pattern. Only variable-binding pattern forms contribute names; wildcard `_`, literals, `Pin`, and `TypeAssert` patterns bind nothing.

For each arm:
1. If the pattern binds names: `enter_scope(bound_names)`.
2. Walk guard (if present) inside the scope.
3. Walk all body expressions inside the scope.
4. `exit_scope()` (if scope was opened).

Wildcard arms open no scope — a VarRef in a wildcard arm body that references a wildcard-bound name has no entry in the resolution table.

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

Escaped dict keys (`$x:`) and annotation PropertyDict values (in constructor bodies) are intentionally **not** suppressed — they are runtime expressions and must resolve correctly.

The wildcard `_` is also never added to `unresolved` even at `suppress_depth == 0`.

---

## Special Cases

### `method_to_instance` — class method fallback

When `resolve_name` fails for a name that looks like a class method (e.g., `+`, `=`), the resolver falls back to scanning all scopes for an instance binding whose method component matches. Instance binding names have the format `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽` or `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⧽`. The resolver resolves to the first match.

The type checker overrides this with the specific instance via `CallDispatch` (a separate `OnceLock` on the same `VarRef` node) when it can determine the correct instance. The resolver's best-effort fallback ensures the OnceLock is set and the lowerer doesn't emit "undefined variable" for method names.

### Leading-dot Field (`.name`)

```
.name   →   SurfaceExpression::Field { expr: None, field: DotKey::Ident(name), resolution, .. }
```

Resolves `name` in the current scope to get the de Bruijn **level** (slot is discarded). The lowerer uses the level with hardcoded root slot constants (`FIELD_GET_ROOT_SLOT`, `SLOT_GET_ROOT_SLOT`) because leading-dot always resolves to a builtin in the root env. If `field-get` is not in scope (resolver not seeded with the runtime env), the `OnceLock` is left unset and the lowerer falls back to `(MAX, MAX)`.

### `expr.field` Field access

Resolves `field-get` in scope to get the level (slot discarded). Same purpose: establishes the de Bruijn level for the root builtin lookup.

### Quote

Variables inside a `Quote` node are AST data, not runtime references. The resolver does not walk into `Quote` at all.

### TypeAlias body

Constructor names, field type expressions, and type parameters are type-level — walking them would produce false "undefined variable" errors. The resolver surgically walks **only** annotation PropertyDict values on constructors, because those are runtime closures stored via `builtin-make-annotated`.

### Or-pattern branches

All branches of an or-pattern must bind the same variable names in the same order. The resolver collects bindings only from the first branch; a `debug_assert` verifies other branches match in debug builds.

---

## Entry Points

```rust
pub fn resolve_surface_program(
    program: &SurfaceProgram,
    initial_frames: &[IndexMap<String, u32>],
) -> (ResolutionTable, Vec<IndexMap<String, u32>>)
```

Resolves all documents in a `SurfaceProgram`. `initial_frames` are scope frames from prior resolver runs (e.g., prelude scope) seeded outermost-first. Returns the `ResolutionTable` and `new_frames` — the frames added by this program, which can be passed as `initial_frames` for a subsequent program.

```rust
pub fn resolve_surface_document_inplace(
    doc: &SurfaceDocument,
    initial_frames: &[IndexMap<String, u32>],
) -> (ResolutionTable, Vec<(String, Span)>, Vec<IndexMap<String, u32>>)
```

Resolves a single document. Also returns `errors` — `(name, span)` pairs for genuinely unresolved expression-position VarRefs. Used by `builtin-resolve` in the meta API for per-document incremental resolution.

Both functions are purely functional with respect to the AST (no mutation visible to callers), but they do write to the inline `Resolution` OnceLocks on each VarRef node.
