# Type Checker

This document is for Rust contributors working in `src/typecheck*.rs` and `src/type_*.rs`. Tinct developers: the type checker produces results written into inline OnceLocks on AST nodes — it does not block evaluation. Type errors are non-fatal; a program with type errors still evaluates, with failed-inference annotations falling back to `Type::Unknown` (accept-all) at runtime.

The type checker walks the Surface AST after desugaring and resolution, performing Hindley–Milner type inference with row polymorphism. It writes its results into inline OnceLocks on AST nodes and into a `TypeAnnotationTable` returned to the caller. Type errors are returned as `Vec<TypeDiagnostic>` — not panics — so evaluation can proceed even when type errors are present.

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
- `state.resolution_table` — a pre-computed `ResolutionTable` (NodeId → (level, slot)) for O(1) VarRef lookup
- The `Env` type environment seeded with builtin types from prior type-checking passes

The type checker writes:
- `TypeAnnotation` OnceLocks on `SurfaceExpression::TypeAssert.resolved_type` nodes — the resolved `Type` for `[@T expr]`
- `TypeAnnotation` OnceLocks on `SurfaceParam.resolved_annotation_type` — the resolved type for each annotated function parameter
- `CallDispatch` OnceLocks on `VarRef` nodes in call position — the mangled instance binding name for typeclass method calls
- `SurfaceNode.type_guard` — a `TypeAnnotation` OnceLock written when the type checker needs to wrap a node with a runtime type assertion that wasn't written by the user
- `InferState.type_annotation_table` — a `TypeAnnotationTable` (NodeId → Type) accumulated during inference, drained into the top-level table by `process_document`

---

## Outputs

### `TypeAnnotationTable`

```rust
pub type TypeAnnotationTable = HashMap<NodeId, Type>;
```

Maps each `TypeAssert` node's `NodeId` to its resolved `Type`. Consumed by the lowering pass to generate `CoreExpr::TypeAssert` with a concrete type instead of `Type::Unknown`. When typecheck is skipped, the lowerer falls back to `Type::Unknown` (accept-all).

Also written to `InferState.type_annotation_table` during inference, then drained into the session-level table by `process_document` after each item.

### `TypeMap`

```rust
pub type TypeMap = HashMap<(u32, u32, u32, u32), Type>;  // (start_line, start_col, end_line, end_col) → Type
```

Span-keyed type map used by the LSP for hover information. Only populated when `enable_hover_map = true` (the LSP path). Not used by the evaluator.

### `CallDispatch` OnceLock

When the type checker determines that a `VarRef` in function call position refers to a typeclass method, it resolves the specific instance and writes the mangled binding name (e.g., `ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽`) into the node's `call_dispatch` OnceLock. The lowerer reads this to rewrite the call to the concrete instance binding, bypassing runtime dispatch.

### `CoreParam.resolved_type`

The type checker resolves each annotated function parameter's type during `infer_fn_push_cont` and writes it to `SurfaceParam.resolved_annotation_type` (a `TypeAnnotation` OnceLock). The lowerer reads this when building `CoreParam`, converting it to `CoreParam.resolved_type: Option<Type>`. `None` means unannotated — the evaluator accepts all values (gradual typing). `Type::Error` (failed inference) is converted to `None` by the lowerer.

This is the mechanism by which type-checker results flow into the evaluator for parameter type guards at function call boundaries.

---

## Entry Points

### `typecheck_surface_program_annotation_table`

```rust
pub async fn typecheck_surface_program_annotation_table(
    program: &SurfaceProgram,
) -> (Vec<TypeDiagnostic>, TypeAnnotationTable, TyConEnv)
```

Thin wrapper — delegates to `typecheck_surface_program_annotation_table_with_env` with an empty initial env, no eval context, no seed tycon env, and a minimal type-stage scope pre-seeded with `Unknown → Type::Unknown`. Used in contexts without a loader pipeline (tests, bootstrap).

### `typecheck_surface_program_annotation_table_with_env`

```rust
pub async fn typecheck_surface_program_annotation_table_with_env(
    program: &SurfaceProgram,
    initial_env: Arc<RwLock<Env>>,
    eval_ctx: Option<Arc<EvalContext>>,
    seed_tycon_env: TyConEnv,
    type_stage_scope: Vec<HashMap<String, TypeStageEntry>>,
) -> (Vec<TypeDiagnostic>, TypeAnnotationTable, TyConEnv)
```

Production entry point used by the loader pipeline. Accepts:
- `initial_env` — base type environment (class/instance/scheme bindings from prior type-checking passes)
- `eval_ctx` — when present, allows `resolve_type_head` to look up type-stage names via scope-chain traversal and `normalize()` to evaluate `TypeStageApp` nodes
- `seed_tycon_env` — accumulated type constructor definitions from prior documents (e.g., `DirCap`, `File` from `builtin_core.llt`)
- `type_stage_scope` — pre-evaluated type-stage scope chain (`Vec<HashMap<String, TypeStageEntry>>`); each entry is a scope frame, Vec[0] = innermost. The caller supplies the complete chain; the function assigns it directly to `state.type_stage_scope`.

The caller supplies the complete type-stage scope chain as a pre-populated `Vec<HashMap<String, TypeStageEntry>>`. The function assigns this directly to `state.type_stage_scope` — no seeding or merging is performed inside this function.

Note: creates a fresh `InferState::new()` (not seeded from `initial_env`). The `initial_env` is used only as the starting `env` variable passed to `process_document` for each document — it is NOT wired into `state.env`. Class/instance lookups during inference go through `state.env`, which starts as a fresh empty `Env`. This is distinct from `typecheck_surface_program_with_env`, which calls `InferState::with_env(child_env)`.

Runs `resolve::resolve_surface_program` before type-checking to populate `state.resolution_table`, enabling O(1) de Bruijn slot lookups during VarRef inference. When an `eval_ctx` is provided, seeds the resolver from the live scope arena's root frame so instance binding names (ɪ-prefixed) are visible.

### `typecheck_surface_program_with_env`

```rust
pub async fn typecheck_surface_program_with_env(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
    enable_hover_map: bool,
    seed_tycon_env: HashMap<String, Arc<TyConDef>>,
    eval_ctx: Option<Arc<EvalContext>>,
    type_stage_scope_override: Option<Vec<HashMap<String, TypeStageEntry>>>,
) -> (Vec<TypeDiagnostic>, TypeMap, DocMap, SchemeMap, InferState, Arc<RwLock<Env>>, TypeAnnotationTable)
```

Extended entry point returning all intermediate state. Used by the loader pipeline's `builtin-typecheck-doc` path and the bootstrap type environment builder (`imports.rs`). The returned `TypeMap` and `DocMap` are empty unless `enable_hover_map = true`.

The `type_stage_scope_override` parameter allows callers to provide a pre-evaluated type-stage scope (e.g., from evaluating `stage: "type"` documents). When `None`, defaults to a minimal scope containing only `Unknown → Type::Unknown`. The bootstrap path in `imports.rs` evaluates the type-stage section of `builtin_core.llt` and passes the result here so that `@Integer`, `@String`, `@Bytes`, etc. resolve correctly during typecheck.

After type-checking all documents, calls `merge_env_schemes_into_env` to flatten the document-level Env frame chain back into the child Env so callers holding the returned Env can see all new bindings.

### `typecheck_surface_program`

```rust
pub async fn typecheck_surface_program(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
) -> (Vec<TypeDiagnostic>, TypeMap, DocMap, SchemeMap)
```

LSP-compatible entry point. Delegates to `typecheck_surface_program_with_env` with `enable_hover_map = true`. Populates the `TypeMap` (span-keyed, for hover) and `SchemeMap` (polymorphic type display). Not used by the normal eval pipeline.

---

## Type-Stage Mechanism

The type-stage mechanism allows type annotations to reference names computed at evaluation time, not hardcoded in Rust. A document section marked `--- stage: "type"` is evaluated before type-checking and its resulting scope is threaded into the type checker as a side channel.

### Two-Pass Structure

The two passes in the type-stage threading are:

**Pass 1 (type-stage evaluation):** The loader evaluates the `--- stage: "type"` section of each module via `builtin-tc-update-type-stage-env`. This call:
1. Evaluates the type-stage document section as a regular tinct program, producing an env Dict
2. Prepends a new frame to `TypeContextData.type_stage_scope` (Vec[0] = innermost) with TypeStageEntry values derived from the env Dict

**Pass 2 (type-stage-aware type-checking):** When `builtin-typecheck-doc` type-checks a module, it reads `TypeContextData.type_stage_scope` from the TypeContext into `InferState.type_stage_scope`. The type checker uses this scope chain as the authoritative source for annotation name resolution:

```
TypeContextData.type_stage_scope
    → InferState.type_stage_scope
        → resolve_type_head (single scope-chain loop)
            → TypeStageEntry::Resolved | Function | TypeVar | Class
                → call_strict_resolver → Type
```

The `type_stage_scope` Vec IS the authoritative type-stage environment. There is no translation layer — each frame is a `HashMap<String, TypeStageEntry>` populated by `builtin-tc-update-type-stage-env` from the evaluated env Dict.

**`builtin-typecheck-doc` third argument — doc-env GroupSpine protocol (T-1891):**

`builtin-typecheck-doc` requires exactly 3 arguments: the resolved Document, the TypeContext, and the doc-env Dict. The doc-env Dict is the same Dict passed to `builtin-eval` when evaluating the same document. The implementation in `src/builtins_meta.rs` (`builtin_typecheck_doc`) builds a `GroupSpine` from this Dict and stores it in `state.type_stage_eval_group`. This GroupSpine is then used as the EvalFrame root scope inside `eval_type_stage_expr` so that type-stage VarRefs (LGM addresses assigned by `builtin-resolve`) resolve correctly against the accumulated loader environment. It also builds `state.scope_frames` from the doc-env Dict's string keys so that `check_constraints_on_var` can resolve instance binding mangled names for typeclass dispatch.

All call sites in `loader.llt`, `test-loader.llt`, and `prelude.llt` pass the doc-env Dict as the third argument. The `type_stage_eval_group` field on `InferState` (see the InferState Fields table) holds the GroupSpine for the duration of the type-checking pass.

### `resolve_type_head` Lookup Order

When the type checker encounters an uppercase annotation name (e.g., `@Integer`, `@Seq`, `@DirCap`), it resolves it through `resolve_type_head` via a single scope-chain loop:

1. **`annotation_scope`** — TypeVar entries from `state.kind_env()` (Operator/Label-kinded names, prepended by the call site before entering the loop)
2. **`state.type_stage_scope`** — frames populated by `builtin-tc-update-type-stage-env` (innermost = Vec[0], highest priority):
   - `TypeStageEntry::Resolved(Type)` → return the Type (with App wrapping if args present)
   - `TypeStageEntry::Function(Arc<Thunk>)` → invoke via `evaluate_resolver_with_thunk` with args
   - `TypeStageEntry::TypeVar(Kind)` → fresh TypeVar of the given kind; break from loop
   - `TypeStageEntry::Class(ClassDecl)` → fresh TypeVar with class constraint; break from loop
3. **Undefined** — returns a `TypeError`

This is the mechanism by which `@Integer` resolves to `Type::Int` in user code: `builtin_core.llt`'s type-stage section defines `Integer = TypeNode.Int`, the loader evaluates it and wires the scope via `builtin-tc-update-type-stage-env`, and `resolve_type_head` finds it in the scope chain.

---

## `process_document`

```rust
pub(crate) async fn process_document(
    doc: &SurfaceDocument,
    parent_env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    table: &mut TypeAnnotationTable,
    type_map: &mut Option<&mut TypeMap>,
) -> (Arc<RwLock<Env>>, Type, Vec<TypeDiagnostic>)
```

Type-checks a single `SurfaceDocument`. Processes all items (Decls and Exprs) in source order, extending the env incrementally.

**Intermediates** (all but the last item): For each intermediate item:
- If `SurfaceExpression::Dict`: calls `run_typecheck_dict` directly to get schemes with let-generalization, extends env with those schemes
- Otherwise: calls `run_typecheck` at `level + 1`, then generalizes the result type if it is a `Type::Dict` and extends env

**Last item**: Treated identically to intermediates except the resulting schemes are placed in the `result_env` (not discarded).

After processing, drains `state.type_annotation_table` into the top-level `table` after each item, and registers type aliases in the env via `register_type_aliases_env`.

Returns `(result_env, result_type, errors)`. The `result_env` is always parented to `parent_env` (flat chain invariant — avoids unbounded scope chain growth).

---

## Dict Type Inference — Multi-Pass Algorithm

Dict entries form a letrec-scoped mutual recursion group. `run_typecheck_dict` in `src/typecheck_cek.rs` is the canonical implementation, called from:
- `DictPassZero` handler in `apply_cont`
- `process_document` (top-level dict expressions)
- `infer_step::Sequential` (intermediate dict bodies)

The algorithm uses Tarjan's SCC decomposition to find groups of mutually dependent entries, then infers each SCC together:

**Pass 0 — Key resolution:** `entry_key_name` resolves the key for each entry (auto-index, string literal, or VarRef). Runs synchronously in the `Dict` arm of `infer_step` before pushing `DictPassZero`.

**Pass 1 — SCC decomposition + fresh TypeVar allocation:** `compute_sccs()` runs Tarjan's algorithm (iterative, not recursive, to avoid Rust stack overflow on large prelude dicts). `collect_dependencies` does a worklist walk of each entry's value AST to identify references to sibling bindings. Fresh TypeVars are allocated for all entries at `level + 1`.

**Pass 2 — Type alias registration:** `[type ...]` declarations in the dict are registered in `dict_env` as `TypeAlias` stubs with `body: Type::Unknown`, so forward references within the same dict resolve correctly. Real bodies are filled in during inference.

**Pass 3 — Letrec inference per SCC:** Each SCC is inferred together. Mutually recursive entries get fresh type variables; their types are unified as uses are encountered. `local_subst` carries per-SCC constraints; bindings propagate into `state.subst` after each entry so sibling entries can see resolved types.

**Pass 4 — Generalization:** After all SCCs are processed, entries are generalized (let-generalization produces `TypeScheme`). Entries that are part of a recursive SCC are not generalized (value restriction — recursive bindings may not be polymorphic).

Returns `(dict_type, schemes, errors)`:
- `dict_type`: the inferred `Type::Dict` for the whole dict
- `schemes`: `IndexMap<String, TypeScheme>` — the generalized schemes for all entries (used by the caller to extend the env)
- `errors`: type errors (non-fatal)

---

## Type Error Model

Type errors are `Vec<TypeDiagnostic>` returned alongside results — not exceptions or panics. The type checker continues inference even after errors, producing partial results. The evaluator can run on a program with type errors; `TypeAssert` nodes whose type failed to infer use `Type::Unknown` (which accepts any value at runtime).

`TypeDiagnostic` is a higher-level diagnostic (T010/T011/T012/T013 quality warnings) produced alongside type errors. These are informational and do not block evaluation. T013 (ambiguous constraint warnings) fire during constraint discharge when a TypeVar escapes generalization with unresolved type class constraints.

---

## Inline OnceLock Protocol

The type checker writes these inline OnceLocks during its walk:

| OnceLock | Location | Written when |
|---|---|---|
| `TypeAnnotation` | `SurfaceNode.type_guard` | Type checker needs to add a runtime assertion not written by the user (gradual typing boundary) |
| `TypeAnnotation` | `SurfaceExpression::TypeAssert.resolved_type` | Resolving the type of a `[@T expr]` annotation |
| `TypeAnnotation` | `SurfaceParam.resolved_annotation_type` | Resolving an annotated function parameter's type (written by `infer_fn_push_cont`) |
| `CallDispatch` | `SurfaceExpression::VarRef.call_dispatch` | Resolving a typeclass method call to a specific instance |
| `MatchableBinding` | `SurfaceMatchArm.guard_matchable_binding` | Resolving the `to-match` Matchable instance for a predicate pattern |

All of these are `OnceLock<Option<...>>` — written at most once. If the type checker does not run (e.g., `--no-typecheck`), all OnceLocks remain at their empty defaults and the lowerer falls back to safe behavior (`Type::Unknown` for type assertions, name-based dispatch for method calls).

The lowerer reads `SurfaceParam.resolved_annotation_type` to populate `CoreParam.resolved_type: Option<Type>`. The evaluator reads `CoreParam.resolved_type` to enforce parameter type guards at call sites. `Type::Error` (failed inference) is converted to `None` (accept-all) by the lowerer — inference failures do not become runtime rejections.

---

## Invariants

1. **Read-only with respect to name resolution.** The type checker reads `Resolution` OnceLocks written by the resolver but never writes to them.
2. **Type errors do not abort.** Inference continues after encountering a type error; the partial result is still returned.
3. **OnceLocks are written at most once per node instance.** `Clone` on `TypeAnnotation` resets to empty — cloned nodes must be re-checked.
4. **SCC algorithm is iterative.** `compute_sccs()` uses an explicit work stack, not Rust recursion, to handle large prelude dicts without stack overflow.
5. **`Type::Unknown` is the accept-all fallback.** When typecheck is skipped or inference fails for a node, `Type::Unknown` causes the runtime `TypeAssert` check to pass for any value.
6. **`type_annotation_table` is drained, not cloned.** `state.type_annotation_table` is drained into the session-level table after each item in `process_document`. Callers must not read it between `run_typecheck` calls without draining.
7. **Env chain is flat at document boundaries.** `process_document` always creates `result_env` with `parent = parent_env`, not the final intermediate env. This prevents unbounded scope chain growth across sequential documents.

---

# Type Checker CEK Machine

The type checker is implemented as an iterative CEK machine (`src/typecheck_cek.rs`) that converts the recursive `infer_surface_expr` tree walk into a loop with an explicit continuation stack. Like the evaluator (`src/eval_materialize.rs`), it uses heap-allocated continuations to eliminate Rust stack recursion and provide an inspectable inference state.

---

## Overview

The naive recursive implementation of type inference suffers from the same problem as the evaluator: deeply nested expressions cause Rust stack overflow. The CEK machine solves this by defunctionalizing the recursive calls — each point where the recursive checker would call itself is instead encoded as a `TypeCheckCont` pushed onto a `Vec<TypeCheckCont>` stack, and the loop processes continuations iteratively.

The machine has three components:

- **Control (C):** the current `Arc<SurfaceNode>` being inferred
- **Environment (E):** the current `Arc<RwLock<Env>>` type environment
- **Kontinuations (K):** a `Vec<TypeCheckCont>` stack of pending work

The "register" that flows between steps is `Type` — the inferred type of the most recently processed node. `infer_step` produces a `Type` from a leaf node directly or pushes a continuation and hands off a child node. `apply_cont` receives the `Type` from a child and either pushes more continuations to continue processing siblings, or returns the final type for this branch.

---

## TypeCheckAction

`TypeCheckAction` is the two-variant enum that controls the main loop:

```rust
pub(crate) enum TypeCheckAction {
    Done(Type),                                    // leaf: inference complete for this node
    Eval(Arc<SurfaceNode>, Arc<RwLock<Env>>),      // compound: evaluate this child node next
}
```

This mirrors the evaluator's `Action` enum (`Continue` / `Materialize`):

| Evaluator (`Action`) | Type Checker (`TypeCheckAction`) |
|---|---|
| `Continue(Ok(v))` | `Done(Type)` |
| `Materialize { thunk }` | `Eval(node, env)` |

`Done` corresponds to a value being ready; `Eval` corresponds to a sub-thunk needing to be forced. In both machines, the loop pops a continuation and applies it when the "ready" variant arrives.

---

## run_typecheck Loop

`run_typecheck` is the main loop. It alternates between `infer_step` and `apply_cont`:

```rust
pub(crate) async fn run_typecheck(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> Type
```

There are two loops. When `infer_step` returns `Eval`, the outer loop immediately calls `infer_step` again on the new node. When `infer_step` returns `Done`, an inner loop drains the continuation stack by repeatedly calling `apply_cont` until either the stack is empty (return the final type) or `apply_cont` returns `Eval` (break back to the outer loop for `infer_step`).

After `infer_step` returns `Done`, the loop calls `record_type_map` to write the inferred type into the `TypeMap` for LSP hover. This happens once per node that produces a `Done` result directly from `infer_step` (leaf nodes and nodes handled entirely in `infer_step` via special-casing).

Both `infer_step` and `apply_cont` are `async fn`. External async operations (annotation resolution, async unify, scope-chain lookup) are awaited directly inline — no special continuation variant exists for them. The CEK loop eliminates recursive calls to `run_typecheck`, not all async behavior.

---

## TypeCheckCont Variants

`TypeCheckCont` is the defunctionalized continuation enum. Each variant stores exactly the data needed to resume inference after a child expression has been processed. The current variants are:

### FnBody

**Pushed by:** the `Fn { params, body }` arm of `infer_step` (via `infer_fn_push_cont`).

**Carries:** `saved_level`, `saved_expected_return`, `return_ann` (pre-resolved return annotation), `params` (fixed positional param types), `typed_variadics` (named variadic buckets with `Seq[T]` types), `rest` (untyped variadic fallback), `required_count`, `node_span`.

**What `apply_cont` does:** receives the body type. Restores `state.level` and `state.expected_return`. Constructs `Type::Function { params, typed_variadics, rest, ret: body_type, required_count }`. Returns `Done(fn_type)`.

### CallFunc

**Pushed by:** the `Call { func, args, named_args }` arm of `infer_step` (general call path, after special-case handling).

**Carries:** `args`, `named_args`, `env`, `span`, `call_node`.

**What `apply_cont` does:** receives the inferred function type. Instantiates the scheme if polymorphic. If there are positional arguments, pushes `CallArg` for the first argument and returns `Eval(args[0], env)`. If there are no arguments, performs arity checking and named-argument unification directly, then returns `Done(return_type)`.

### CallArg

**Pushed by:** `CallFunc` (for the first argument) and by itself (for each subsequent argument).

**Carries:** `idx`, `remaining_args`, `accumulated_arg_types`, `arg_nodes`, `param_types`, `fn_ret`, `typed_variadics`, `rest`, `fn_required`, `env`, `named_args`, `span`, `call_node`.

**What `apply_cont` does:** receives one argument type. Widens literal types (`IntLiteral → Int`, `StringLiteral → Str`) then unifies with the corresponding parameter type using Robinson unification. If `remaining_args` is non-empty, pushes another `CallArg` and returns `Eval`. When all positional arguments are processed, handles named arguments inline (each named arg is unified with the corresponding parameter by name), checks arity against `fn_required`, and returns `Done(fn_ret)`.

This is the single canonical call-checking path — the old `check_call`/`check_call_with_scheme`/`check_call_args` functions have been absorbed here.

### MatchScrutinee

**Pushed by:** the `Match { scrutinee, arms }` arm of `infer_step`.

**Carries:** `arms`, `env`, `span`.

**What `apply_cont` does:** receives the scrutinee type. Runs exhaustiveness checking upfront on all arms via `run_match_exhaustiveness_check`. Sets up the first arm's environment (pattern bindings, guard inference, narrowing) via `setup_match_arm_env`. Pushes `MatchArm` for the first arm and returns `Eval(arms[0].body_expr(), arm_env)`.

### MatchArm

**Pushed by:** `MatchScrutinee` (for the first arm body) and by itself (for each subsequent arm body).

**Carries:** `remaining_arms`, `env`, `accumulated_types`, `scrutinee_ty`, `remaining_scrutinee`, `span`.

**What `apply_cont` does:** receives one arm body type. Appends it to `accumulated_types`. If `remaining_arms` is non-empty, calls `setup_match_arm_env` for the next arm and pushes another `MatchArm`. When all arms are processed, computes the union of `accumulated_types` and returns `Done(union_type)`.

Note: guard inference for each arm still calls `run_typecheck` internally (via `setup_match_arm_env`). Only arm body inference is fully iterative via the `MatchArm` chain.

### DictPassZero

**Pushed by:** the `Dict { entries }` arm of `infer_step`. The Dict arm runs Pass 0 (key name resolution) synchronously, then pushes `DictPassZero` and returns `Done(Type::Unknown)` to immediately trigger `apply_cont`.

**Carries:** `dict_node`, `entries`, `key_entries` (from Pass 0), `env`, `enclosing_level`, `span`.

**What `apply_cont` does:** calls `run_typecheck_dict(entries, env, state, type_map, span)` which runs the full multi-pass SCC-based dict inference. Returns `Done(dict_type)`.

Note: `Done(Type::Unknown)` is returned by the `Dict` arm of `infer_step` as a sentinel to immediately trigger `apply_cont`. This `Unknown` is never propagated as an inference result — it is immediately consumed by the `DictPassZero` handler which replaces it with the actual dict type.

### SequentialNonDictIntermediate

**Pushed by:** the `Sequential(exprs)` arm of `infer_step` when it encounters a non-Dict intermediate body.

**Carries:** `intermediate_span`, `remaining_intermediates`, `last`, `env`, `enclosing_level`.

**What `apply_cont` does:** receives the type of the just-evaluated non-Dict intermediate. If it is `Type::Dict`, extends `env` with generalized schemes. If it is `Type::Unknown | Type::Any`, no extension. Otherwise, records a `not_a_record` error. Processes remaining intermediates: if the next intermediate is a Dict, calls `run_typecheck_dict` synchronously and loops; if it is non-Dict, pushes another `SequentialNonDictIntermediate` and returns `Eval`. When all intermediates are done, returns `Eval(last, env)`.

Note: Dict intermediates inside a `Sequential` are handled synchronously within both `infer_step` and `SequentialNonDictIntermediate` to avoid the overhead of an additional continuation level for the common case.

### TypeAssertInner

**Pushed by:** the `TypeAssert { annotation, expr }` arm of `infer_step`, after resolving `annotation` synchronously (via `resolve_annotation`).

**Carries:** `expected`, `has_default`, `default_node`, `env`, `span`, `annotation_span`.

**What `apply_cont` does:** receives the inner expression type. Checks consistency/subtype against `expected`. On mismatch without a default, records a type error. On mismatch with a default, validates the default's type via a fresh `run_typecheck` call. Returns `Done(expected)` in all cases (the annotation type is the result, not the inferred inner type).

Does NOT write `TypeAnnotation` OnceLocks or `TypeAnnotationTable` entries — that is handled by the top-level `typecheck_surface_program_annotation_table_with_env` path and `process_document`.

### FieldBase

**Pushed by:** the `Field { expr, field }` arm of `infer_step` (when `expr` is `Some`).

**Carries:** `field` (the `DotKey`), `span`.

**What `apply_cont` does:** receives the inferred base expression type. Calls `check_dot_access` (integer key) or `check_dot_access_str` (string key) to resolve the field type. Returns `Done(field_type)`.

### Unquote / UnquoteSplice

**Pushed by:** `Unquote(inner)` and `UnquoteSplice(inner)` arms of `infer_step`.

**What `apply_cont` does:** `Unquote` returns `Done(inner_ty)` (passes the type through). `UnquoteSplice` returns `Done(Type::Unknown)` (splice positions are untyped).

---

## Special-Cased Expressions in `infer_step`

One expression is handled entirely within `infer_step` without pushing a continuation:

- **`ℊꜱʏᴍ⧼do-infer⧽N`** — do-infer sentinel. Returns `Type::Unknown` immediately (the monad resolution happens via `state.do_infer_resolutions` as a side channel).

All other call forms — including `[if cond t f]`, `[get k c]`, and `[get-in path c]` — route through the general `Call → CallFunc → CallArg` CEK path. The `infer_if_expr`, `infer_get_call`, and `infer_get_in_call` special-case functions were deleted in sprints S-985 and S-992.

---

## Call-Checking Unification

Before the CEK machine, call type checking had three separate code paths that had to be kept in sync:

1. **Inline poly approximation** in the Call arm of the old `infer_surface_expr` — used direct `state.subst.type_map.insert()` instead of Robinson unification, producing subtly different results
2. **`check_call_with_scheme`** in `typecheck_call.rs` — the principled polymorphic path
3. **`check_call`** in `typecheck_call.rs` — the general case dispatcher

The CEK machine consolidates all three into a single path through `CallFunc` and `CallArg`. There is no `CALL-MONO`/`CALL-POLY` dispatch — unification handles both cases uniformly. `typecheck_call.rs` now contains only two small helper functions:
- `widen_literal_types(ty: Type) -> Type` — `IntLiteral → Int`, `StringLiteral → Str` before arg unification
- `is_concrete_type(ty: &Type) -> bool` — predicate for gradual typing boundary detection

---

## Dict Inference

Dict type inference is multi-pass. The CEK machine encodes this as:

```
infer_step(Dict)
    → resolve key names synchronously (Pass 0)
    → push DictPassZero
    → Done(Unknown)   ← sentinel to trigger apply_cont immediately

apply_cont(DictPassZero)
    → run_typecheck_dict(entries, env, state, type_map, span)
        → compute_sccs() [Tarjan — iterative, synchronous]
        → allocate fresh TypeVars for all entries (Pass 1)
        → register type aliases in dict_env (Pass 2)
        → for each SCC (in reverse topological order):
            → infer all members together (Pass 3)
            → generalize non-recursive entries (Pass 4)
        → return (dict_type, schemes, errors)
    → Done(dict_type)
```

The `Done(Unknown)` returned by `infer_step(Dict)` is a sentinel to immediately trigger `apply_cont`. It is consumed entirely by `DictPassZero` and never propagates as an inference result.

Both `compute_sccs` and `collect_dependencies` have their canonical implementations in `src/typecheck_cek.rs`. `src/typecheck_dict.rs` now contains only unit tests for `compute_sccs`.

---

## Relationship to the Evaluator

The type checker CEK machine and the evaluator CEK machine (`src/eval_materialize.rs`) follow the same architectural pattern:

| Evaluator | Type Checker |
|---|---|
| `Action` enum | `TypeCheckAction` enum |
| `Continue(Result<Value>)` | `Done(Type)` |
| `Materialize { thunk }` | `Eval(node, env)` |
| `Vec<Cont>` continuation stack | `Vec<TypeCheckCont>` continuation stack |
| `force_step` — produces next Action | `infer_step` — produces next Action |
| `apply_cont` — applies continuation | `apply_cont` — applies continuation |
| `run` — main loop | `run_typecheck` — main loop |
| Result is a `Value` | Result is a `Type` |

The symmetry is deliberate. Both systems convert a recursive tree walk into a loop with an explicit heap-allocated stack. Both use a two-variant "ready/continue" enum to drive the loop.

The key structural difference is that the evaluator operates on `Thunk`s (lazy values with memoization) while the type checker operates on `SurfaceNode`s (AST nodes, not memoized). The evaluator's `Memoize` continuation has no counterpart in the type checker — type inference is a one-pass write, not a cached result.

The type checker communicates results to the evaluator through three channels:
1. **`CoreParam.resolved_type`** — parameter type guards (via `SurfaceParam.resolved_annotation_type` OnceLock → lowerer → `CoreParam`)
2. **`boundary_guards`** — `HashMap<Span, Type>` in `InferState`, read by the evaluator via `EvalContext` to insert runtime type guards at gradual typing boundaries
3. **`do_infer_resolutions`** — `HashMap<String, String>` mapping do-infer sentinel VarRef names to resolved monad variable names; read by the evaluator at runtime to dispatch `[do ...]` forms

---

## InferState Fields

`InferState` in `src/type_infer.rs` is the mutable state threaded through all type inference. Key fields:

| Field | Type | Purpose |
|---|---|---|
| `level` | `u32` | Current let-generalization level (Kiselyov 2013) |
| `levels` | `HashMap<String, u32>` | Per-TypeVar creation level |
| `subst` | `Substitution` | Global accumulated substitution (type_map only; kinded) |
| `type_vars` | `IndexMap<String, TypeVarEntry>` | Unified TypeVar table: level + binding + kind per var |
| `constraints` | `Vec<Constraint>` | Accumulated type class constraints |
| `kind_env` | `HashMap<String, Kind>` | TypeVar kind assignments (Operator, Label) |
| `env` | `Arc<RwLock<Env>>` | Unified Env: classes, instances, schemes |
| `scheme_map` | `Option<SchemeMap>` | VarRef span → TypeScheme (for LSP hover; None = disabled) |
| `expected_return` | `Option<Type>` | Enclosing function's declared return type (for do-infer) |
| `diagnostics` | `Vec<TypeDiagnostic>` | T010/T011/T012/T013 quality warnings |
| `deferred_equalities` | `Vec<(Type, Type)>` | Stuck TypeStageApp equality constraints |
| `boundary_guards` | `HashMap<Span, Type>` | Gradual typing boundary span → expected type |
| `do_infer_resolutions` | `HashMap<String, String>` | do-infer sentinel → resolved monad name |
| `resolution_table` | `Option<Arc<ResolutionTable>>` | Pre-computed NodeId → (level, slot) for O(1) VarRef lookup |
| `eval_ctx` | `Option<Arc<EvalContext>>` | EvalContext for type-stage scope-chain lookup |
| `type_stage_scope` | `Vec<HashMap<String, TypeStageEntry>>` | Type-stage scope chain (Vec[0] = innermost). Populated by bootstrap or builtin-tc-update-type-stage-env |
| `type_stage_eval_group` | `Option<Arc<GroupSpine>>` | Optional doc-env GroupSpine from `builtin-typecheck-doc`'s third argument; used as the EvalFrame root scope in `eval_type_stage_expr` so type-stage VarRefs resolve from the accumulated loader environment. Falls back to `GroupSpine::empty()` when None. |
| `tycon_env` | `HashMap<String, Arc<TyConDef>>` | Type constructor definitions |
| `type_annotation_table` | `TypeAnnotationTable` | Per-session TypeAssert NodeId → Type; drained by `process_document` |
| `expects_resolved` | `HashMap<Span, Type>` | Resolved `--- expects:` contract types |
| `failed_bindings` | `HashMap<String, Span>` | Failed bindings for "caused by" T002 error attribution |

Note: `InferState` contains several fields marked as "compatibility fields" (`type_vars`, `bounds`, `fd_in_progress`, `expansion_stack`, `pending_param_narrowings`) that represent an in-progress migration between two design generations. In the current design, TypeVar bindings are in `subst.type_map`, levels in `levels`, and kinds in `kind_env`. The `type_vars` IndexMap is maintained in parallel for compatibility with `type_class.rs` save/restore probe patterns.

---

## Annotation Resolution (`src/typecheck_annot.rs`)

`resolve_annotation` is the main entry point for converting a tinct `Annotation` AST node into a `Type`. Called from:
- `infer_step::TypeAssert` — to get the expected type before pushing `TypeAssertInner`
- `infer_fn_push_cont` — to resolve parameter and return type annotations

Key functions:

| Function | Purpose |
|---|---|
| `resolve_annotation` | Main dispatcher: simple name, `Fn@...`, `Handle@...`, compound annotation |
| `resolve_type_head` | 5-step lookup for uppercase type names (class → tycon → scope chain) |
| `resolve_type_name` | Lowercase type variable resolution (ann_mapping, type_params_scope, cross-kind) |
| `resolve_fn_type` | `Fn@RetType [Params]` function type syntax |
| `resolve_fn_metadata` | `fn@[return: ... constraint: ... doc: ...]` metadata dict |
| `resolve_annotated` | Dispatcher for `@Name` subscript form (Fn, Handle, bare name) |
| `resolve_type_assert` | TypeAssert resolution: resolves annotation + checks inner with bidirectional checking |
| `infer_variance` | Dolan 2017 polarity analysis for type parameter variance inference |
| `annotation_to_variance` | `Covariant`/`Contravariant`/`Invariant`/`Phantom` → `Variance` |

`resolve_fn_metadata` processes function metadata dict keys in a fixed order:
1. `bind:` — declares TypeVars in `ann_mapping`
2. `kinds:` — registers kind constraints
3. `constraint:` — single-param class constraints (`[a: Comparable]`)
4. Multi-param class constraints (`[$Add a b c]`)
5. `return:` — resolves return type
6. `doc:` — extracts documentation string

---

## Type Normalization (`src/type_normalize.rs`)

`normalize` reduces `TypeStageApp { fn_name, args }` nodes to concrete types when all arguments are ground. It is called from `resolve_type_head` during annotation resolution (when a type constructor application needs to be evaluated).

Steps:
1. Apply current substitution to resolve bound TypeVars
2. If `TypeStageApp`: normalize each arg recursively; if all ground and `allow_eval = true`, walk `NormCtxt.type_stage_scope` (a `Vec<HashMap<String, TypeStageEntry>>` copied from `InferState.type_stage_scope` at call sites in `type_unify.rs`) looking for `fn_name`, and call `evaluate_resolver_with_thunk` / `call_strict_resolver` on the found `TypeStageEntry::Function` thunk
3. Cache the result (ground types only)

`allow_eval` is set to `false` inside `unify()` to prevent resolver evaluation failures from becoming type errors. TypeStageApp nodes that cannot be reduced remain stuck and may be deferred via `state.deferred_equalities`.

`NormCtxt` carries the eval context, depth counter (max 64), cycle detection call stack, and cache. It is created fresh per annotation resolution call via `NormCtxt::new(state.eval_ctx.clone())`.

---

## Unification (`src/type_unify.rs`)

The unification module implements Robinson unification for Hindley-Milner with extensions for type classes and structural record types.

Key functions:

| Function | Purpose |
|---|---|
| `satisfies_constraint(ty, class_name)` | Meta-rules for Unknown/Never/Union/Intersection; returns false for concrete types |
| `entails(class_env, context, target)` | Constraint entailment with superclass transitivity (Jones 1992) |
| `check_constraints_on_var(...)` | Fires when a TypeVar is bound; resolves HasField and class constraints |
| `is_superclass_of(env, sub, sup)` | Transitive superclass check with cycle detection |

Type variable bindings are stored in `InferState.type_vars` (an `IndexMap<String, TypeVarEntry>`). Binding operations go through `InferState.bind_type_var()`; lookups through `InferState.type_vars.get()`. The note in `src/type_unify.rs` states the old `Substitution` struct was "removed" and all binding goes through `InferState`, but the current `InferState` still contains both `subst: Substitution` (for instantiation renaming) and `type_vars: IndexMap<String, TypeVarEntry>` (for the unified table).

Constraint satisfaction for `Unknown` follows AGT existential lifting (Garcia, Clark & Tanter, POPL 2016): `Unknown` vacuously satisfies all constraints because its interpretation set contains at least one instance of every class.

Instance resolution recursion depth is tracked in `state.instance_resolution_depth` to prevent infinite loops through the cycle `check_constraints_on_var → resolve_instance → unify → check_constraints_on_var`. Matches GHC's `-freduction-depth` semantics (Sulzmann et al. 2007 §3.2).

---

## Layering Notes

The type checker interacts with other subsystems through well-defined channels. However, there are some layering issues worth noting:

1. **`InferState` carries dual substitution stores.** `state.subst` (a `Substitution` with `type_map: RefCell<HashMap>`) is used for instantiation renaming and access-chain constraint accumulation. `state.type_vars` (an `IndexMap<String, TypeVarEntry>`) is used by `type_class.rs` for save/restore probe patterns. These are parallel stores for overlapping data — the relationship is not fully normalized. This is a known compatibility issue during an in-progress design migration.

2. **`type_unify.rs` module comment claims `Substitution` was removed**, but `InferState.subst` still exists and is actively used (by `Substitution::apply` during instantiation). The module comment is aspirational, not current.

3. **`infer_step::Sequential` calls `run_typecheck_dict` synchronously** for Dict intermediates to avoid an extra continuation level. This is correct but means Dict inference is not fully lazy within a Sequential — it runs before the non-Dict intermediates in the continuation queue are processed.

4. **`infer_step` no longer special-cases `if`, `get`, or `get-in`.** The `infer_if_expr`, `infer_get_call`, and `infer_get_in_call` special-case functions were deleted in sprints S-985 and S-992. All three call forms now route through the general `Call → CallFunc → CallArg` CEK path. No recursive `Box::pin(run_typecheck(...))` calls remain in `infer_step`.

5. **Two parallel type-checking paths.** `typecheck_surface_program_annotation_table_with_env` (eval pipeline) and `typecheck_surface_program_with_env` (LSP + loader pipeline) share `process_document` and `run_typecheck_dict` but have different entry-point signatures and seed different parts of `InferState`. A caller that wants both the `TypeAnnotationTable` and the full LSP output must use `typecheck_surface_program_with_env` and extract the `annotation_table` from its return tuple.

---

## File Locations

| File | Role |
|---|---|
| `src/typecheck_cek.rs` | `TypeCheckCont`, `TypeCheckAction`, `run_typecheck`, `infer_step`, `apply_cont`, `run_typecheck_dict`, `compute_sccs`, `collect_dependencies`, `entry_key_name`, `infer_fn_push_cont`, `infer_var_ref` |
| `src/typecheck.rs` | Top-level entry points (`typecheck_surface_program_annotation_table`, `typecheck_surface_program_annotation_table_with_env`, `typecheck_surface_program_with_env`, `typecheck_surface_program`); `process_document`; `merge_env_schemes_into_env`; `register_type_aliases_env`; module declarations for all typecheck submodules |
| `src/typecheck_dict.rs` | Unit tests for `compute_sccs`. Dict inference is fully in `typecheck_cek::run_typecheck_dict`. |
| `src/typecheck_call.rs` | `widen_literal_types`, `is_concrete_type` — two small helpers retained after CEK machine migration absorbed all call-checking logic |
| `src/typecheck_annot.rs` | Annotation resolution: `resolve_annotation`, `resolve_type_head`, `resolve_type_name`, `resolve_fn_type`, `resolve_fn_metadata`, `resolve_annotated`, `resolve_type_assert`, `infer_variance`, `annotation_to_variance` |
| `src/typecheck_narrow.rs` | Path-sensitive narrowing: `extract_param_indices`, `extract_pattern_types`, `patterns_overlap`, `types_can_unify` |
| `src/typecheck_match.rs` | Match arm type inference: `setup_match_arm_env`, `run_match_exhaustiveness_check`, `build_case_arm_env` |
| `src/typecheck_diag.rs` | T010/T011/T012 type quality diagnostics: `scan_type_quality`, `scan_explicit_unknown_t011` |
| `src/type_infer.rs` | `TypeVarEntry`, `Substitution`, `TypeScheme`, `SchemeMap`, `TypeStageEntry`, `InferState` |
| `src/type_unify.rs` | `satisfies_constraint`, `entails`, `check_constraints_on_var`, `is_superclass_of`, Robinson unification, functional dependency improvement |
| `src/type_normalize.rs` | `NormCtxt`, `normalize`, `call_strict_resolver`, `type_to_typenode` |
