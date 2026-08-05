# Expand

This document is for Rust contributors working in `src/expand.rs`. Tinct developers should be aware that after this pass, `[type ...]`, `[class ...]`, and `[instance ...]` declarations no longer exist in the AST — they have been replaced by `TypeDecl`, `UnitVariant`, `MakeVariant`, and `ReprDecl` nodes that the resolver and lowerer handle uniformly.

Expansion is a structural AST-to-AST transformation on the `SurfaceProgram` that runs after desugaring and before name resolution. It converts high-level declaration nodes into equivalent expression trees so the resolver sees all entries and assigns slots uniformly.

---

## Pipeline Position

```
parse → desugar → expand → resolve → typecheck → lower → eval
```

---

## Why This Stage Exists

The lowerer used to create synthetic `CoreEntry` nodes for type constructors, instance methods, and spread-dict merges. These entries bypassed the resolver's slot assignment entirely — they were injected after resolution with sentinel slot values (`u32::MAX`). This produced three problems:

1. **Sentinels** — `u32::MAX` slots required special-case handling in the evaluator.
2. **`scope_frames` parameter** — `lower()` needed a mutable scope-tracking parameter to allocate slots for entries the resolver never saw.
3. **Parallel implementations** — the lowerer and resolver maintained independent entry-creation logic that could diverge.

Moving expansion before resolution eliminates all three. The expand stage produces ordinary `SurfaceExpression` entries that the resolver processes like any user-written code.

---

## Transformation Catalog

### TypeAlias -> TypeDecl wrapping constructor entries

`[type Red Green Blue]` expands to a `TypeDecl` node wrapping a `Dict` of constructor entries:

- **Unit constructors** (bare uppercase words like `Red`) become `UnitVariant { tycon, ctor, type_decl_id }` entries.
- **Payload constructors** (named entries like `File: [path: String]`) become `Fn` nodes whose body is a `MakeVariant { tag, payload, type_decl_id }`. The function accepts the constructor's fields as parameters and builds the payload dict.

When `repr:` metadata is present on the type body, the constructor dict is wrapped in a `ReprDecl { repr, inner }` before the outer `TypeDecl`. When constructors carry `@[...]` annotations, they are wrapped in `[builtin-make-annotated expr ann_dict]` calls.

Each `[type ...]` declaration receives a unique `type_decl_id` from a global atomic counter (starting at 100, above the reserved primitive range 1-99). This ID threads through `TypeDecl`, `UnitVariant`, and `MakeVariant` so the evaluator can distinguish same-named types in different scopes (B-714).

### InstanceDecl -> fanned-out method entries

`[instance Addable [let a@String b@String c]: [+: [fn ...]]]` fans out into individual method entries at the dict level. For each arm, the expand stage:

1. Extracts type annotations from the arm pattern (`a@String`, `b@String`).
2. Injects those annotations as type guards into each method function's parameters.
3. Emits one `SurfaceEntry` per method.

Named instances also emit an outer key binding pointing to the methods dict, preserving runtime access like `PrintableStr.print`.

### ClassDecl -> empty dict placeholder

`[class [let a]]` becomes an empty `Dict([])`. The class name occupies a resolver slot so downstream code can reference it, but no runtime value is needed.

### Spread-dict -> nested builtin-dict-merge calls

`[a: 1  ...rest  b: 2]` is rewritten to nested `Call(VarRef("builtin-dict-merge"), ...)` expressions. Entries are segmented around spread markers, then folded left-associatively:

```
Call(VarRef("builtin-dict-merge"),
  [Call(VarRef("builtin-dict-merge"), [Dict([a: 1]), rest]),
   Dict([b: 2])])
```

The `builtin-dict-merge` reference is an ordinary `VarRef` — the resolver finds it naturally.

---

## New SurfaceExpression Variants

The expand stage introduces four `SurfaceExpression` variants that never appear in parser output:

| Variant | Represents | Lowered to |
|---------|-----------|------------|
| `UnitVariant { tycon, ctor, type_decl_id }` | A unit constructor value (e.g., `Color.Red`) | `CoreExpr::UnitVariant` (1:1) |
| `MakeVariant { tag, payload, type_decl_id }` | A payload variant construction (body of a constructor fn) | `CoreExpr::Variant` (1:1) |
| `TypeDecl { type_name, type_decl_id, inner }` | Wrapper that registers a type identity at eval time | `CoreExpr::TypeDecl` (1:1) |
| `ReprDecl { repr, inner }` | Wrapper that associates a `repr:` string with a type's constructor dict | `CoreExpr::ReprDecl` (1:1) |

All four are marked `#[expr(skip)]` in the AST — they are transparent to the resolver's scope analysis. The resolver recurses into their children (`MakeVariant.payload`, `TypeDecl.inner`, `ReprDecl.inner`) but does not treat the wrapper nodes as scope boundaries. `UnitVariant` is a leaf with no children to resolve.

---

## Payload Constructor Leading-Dot Pattern

Payload constructors expand to functions like `[fn [let path handle] [path: .path  handle: .handle]]`. The leading-dot `.path` syntax is the key detail: inside the payload dict, a bare `path` would be a letrec self-reference (the dict entry referring to itself). Leading-dot `.path` skips the dict's letrec scope and resolves from the parent scope — the `fn`'s parameter scope. This is the same pattern a tinct programmer would write by hand:

```tinct
[fn [let types] [types: .types]]
```

The resolver handles leading-dot fields by looking up the name one scope level above the current dict.

---

## Cross-References

- `doc/11a-builtins.md` — `builtin-expand` pipeline builtin
- `src/expand.rs` — implementation
- `src/ast.rs` — `UnitVariant`, `MakeVariant`, `TypeDecl`, `ReprDecl` variant definitions
- `doc/internals/desugar.md` — preceding pipeline stage
- `doc/internals/name-resolution.md` — following pipeline stage
