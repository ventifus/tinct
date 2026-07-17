# Type Checker

The type checker walks the Surface AST after desugaring and resolution, performing Hindley–Milner type inference with row polymorphism. It writes its results into inline OnceLocks on AST nodes and into a `TypeAnnotationTable` returned to the caller. Type errors are returned as `Vec<TypeError>` — not panics — so evaluation can proceed even when type errors are present.

---

## Pipeline Position

```
Parse → Desugar → Resolve → Typecheck ← here
                                 ↓
                         TypeAnnotationTable    (NodeId → Type, for lowering)
                         TypeMap               (Span → Type, for LSP)
                         TypeAnnotation OnceLocks on TypeAssert nodes
                         CallDispatch OnceLocks on typeclass call VarRef nodes
```

The type checker reads:
- `Resolution` OnceLocks set by the resolver (de Bruijn coords for variable lookup)
- The `Env` type environment seeded with builtin types

The type checker writes:
- `TypeAnnotation` OnceLocks on `TypeAssert` nodes — the inferred/resolved `Type` for `[@T expr]`
- `CallDispatch` OnceLocks on `VarRef` nodes in call position — the mangled instance binding name for typeclass method calls
- `SurfaceNode.type_guard` — a `TypeAnnotation` OnceLock written when the type checker needs to wrap a node with a runtime type assertion that wasn't written by the user

---

## Outputs

### `TypeAnnotationTable`

```rust
pub type TypeAnnotationTable = HashMap<NodeId, Type>;
```

Maps each `TypeAssert` node's `NodeId` to its resolved `Type`. Consumed by the lowering pass to generate `CoreExpr::TypeAssert` with a concrete type instead of `Type::Unknown`. When typecheck is skipped, the lowerer falls back to `Type::Unknown` (accept-all).

### `TypeMap`

```rust
pub type TypeMap = HashMap<(usize, usize), Type>;  // (start_offset, end_offset) → Type
```

Span-keyed type map used by the LSP for hover information and import resolution. Not used by the evaluator.

### `CallDispatch` OnceLock

When the type checker determines that a `VarRef` in function call position refers to a typeclass method, it resolves the specific instance and writes the mangled binding name (e.g., `ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽`) into the node's `call_dispatch` OnceLock. The lowerer reads this to rewrite the call to the concrete instance binding, bypassing runtime dispatch.

---

## Entry Points

### `typecheck_surface_program_annotation_table`

```rust
pub async fn typecheck_surface_program_annotation_table(
    program: &SurfaceProgram,
) -> (Vec<TypeError>, TypeAnnotationTable, TyConEnv)
```

The standard entry point for the evaluation pipeline. Returns the `TypeAnnotationTable` (for lowering), type errors, and accumulated `TyConEnv` (type constructor definitions). Uses the builtin core type env as the seed.

### `typecheck_surface_program_annotation_table_with_env`

```rust
pub async fn typecheck_surface_program_annotation_table_with_env(
    program: &SurfaceProgram,
    initial_env: Arc<RwLock<Env>>,
    eval_ctx: Option<Arc<EvalContext>>,
    type_stage_env: Option<Arc<RwLock<Env>>>,
    seed_tycon_env: TyConEnv,
    type_stage_thunks: Option<HashMap<String, ThunkId>>,
) -> (Vec<TypeError>, TypeAnnotationTable, TyConEnv)
```

Extended entry point used by the loader pipeline. Accepts:
- `initial_env` — base type environment (builtin types already included)
- `eval_ctx` — when present, allows the type normalizer to evaluate `TypeStageApp` nodes using the runtime evaluator (for the type-stage mechanism)
- `type_stage_env` — type-level builtins evaluated in the type-stage pass
- `seed_tycon_env` — accumulated type constructor definitions from prior documents

### `typecheck_surface_program`

```rust
pub async fn typecheck_surface_program(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
) -> (Vec<TypeError>, TypeMap, DocMap, SchemeMap, Vec<TypeDiagnostic>)
```

LSP-compatible entry point. Returns a `TypeMap` keyed by span offsets (for hover), a `DocMap` (variable name → documentation string), and a `SchemeMap` (for polymorphic type display in hover). Not used by the normal eval pipeline.

---

## Dict Type Inference — Multi-Pass Algorithm

Dict entries form a letrec-scoped mutual recursion group. The type checker uses Tarjan's SCC algorithm to find groups of entries with mutual dependencies, then infers each SCC together:

**Pass 1 — SCC decomposition:** `compute_sccs()` runs Tarjan's algorithm (iterative, not recursive, to avoid Rust stack overflow on large dicts) on the entry dependency graph. Returns SCCs in reverse topological order (dependencies before dependents).

**Pass 2 — Mono inference per SCC:** Each SCC is inferred together. Mutually recursive entries get fresh type variables; their types are unified as uses are encountered.

**Pass 3 — Generalization:** After all SCCs are processed, non-recursive entries are generalized (let-generalization produces type schemes). Entries that are part of a recursive SCC are not generalized (the value restriction — recursive bindings may not be polymorphic).

**Pass 4 — Final annotation:** The inferred types for `TypeAssert` nodes are written into `TypeAnnotationTable` and the inline `TypeAnnotation` OnceLocks.

This multi-pass approach correctly infers types for mutually recursive functions in a dict without requiring explicit type annotations.

---

## Type Error Model

Type errors are `Vec<TypeError>` returned alongside results — not exceptions or panics. The type checker continues inference even after errors, producing partial results. The evaluator can run on a program with type errors; `TypeAssert` nodes whose type failed to infer use `Type::Unknown` (which accepts any value at runtime).

`TypeDiagnostic` is a higher-level diagnostic (T010/T011/T012 quality warnings) produced alongside type errors. These are informational and do not block evaluation.

---

## Inline OnceLock Protocol

The type checker writes these inline OnceLocks during its walk:

| OnceLock | Location | Written when |
|---|---|---|
| `TypeAnnotation` | `SurfaceNode.type_guard` | Type checker needs to add a runtime assertion not written by the user |
| `TypeAnnotation` | `SurfaceExpression::TypeAssert.resolved_type` | Resolving the type of a `[@T expr]` annotation |
| `TypeAnnotation` | `Pattern::TypeAssertPending.resolved` | Resolving a type pattern `n@T` in a match arm |
| `CallDispatch` | `SurfaceExpression::VarRef.call_dispatch` | Resolving a typeclass method call to a specific instance |
| `MatchableBinding` | `SurfaceMatchArm.guard_matchable_binding` | Resolving the `to-match` Matchable instance for a predicate pattern |

All of these are `OnceLock<Option<...>>` — written at most once. If the type checker does not run (e.g., `--no-typecheck`), all OnceLocks remain at their empty defaults and the lowerer falls back to safe behavior (`Type::Unknown` for type assertions, name-based dispatch for method calls).

---

## Invariants

1. **Read-only with respect to name resolution.** The type checker reads `Resolution` OnceLocks written by the resolver but never writes to them.
2. **Type errors do not abort.** Inference continues after encountering a type error; the partial result is still returned.
3. **OnceLocks are written at most once per node instance.** `Clone` on `TypeAnnotation` resets to empty — cloned nodes must be re-checked.
4. **SCC algorithm is iterative.** `compute_sccs()` uses an explicit work stack, not Rust recursion, to handle large prelude dicts without stack overflow.
5. **`Type::Unknown` is the accept-all fallback.** When typecheck is skipped or inference fails for a node, `Type::Unknown` causes the runtime `TypeAssert` check to pass for any value.
