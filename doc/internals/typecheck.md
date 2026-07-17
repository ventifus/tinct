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
    type_stage_map: Option<HashMap<String, TypeStageEntry>>,
) -> (Vec<TypeError>, TypeAnnotationTable, TyConEnv)
```

Extended entry point used by the loader pipeline. Accepts:
- `initial_env` — base type environment (builtin types already included)
- `eval_ctx` — when present, allows the type normalizer to evaluate `TypeStageApp` nodes using the runtime evaluator (for the type-stage mechanism)
- `type_stage_env` — type-level builtins evaluated in the type-stage pass
- `seed_tycon_env` — accumulated type constructor definitions from prior documents
- `type_stage_map` — pre-computed type-stage entries (TypeStageEntry::Resolved for materialized types, TypeStageEntry::Function for parameterized type constructors)

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

---

# Type Checker CEK Machine

The type checker is implemented as an iterative CEK machine that converts the recursive `infer_surface_expr` tree walk into a loop with an explicit continuation stack. Like the evaluator (`src/eval_materialize.rs`), it uses heap-allocated continuations to eliminate Rust stack recursion and provide an inspectable inference state.

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
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> Type {
    let mut current_node = Arc::clone(node);
    let mut current_env = Arc::clone(env);

    loop {
        match infer_step(&current_node, &current_env, state, errors, type_map, stack).await {
            TypeCheckAction::Eval(next_node, next_env) => {
                current_node = next_node;
                current_env = next_env;
            }
            TypeCheckAction::Done(ty) => {
                // Inner loop: drain the continuation stack until we get an Eval or the stack empties.
                let mut result_ty = ty;
                loop {
                    match stack.pop() {
                        None => return result_ty,
                        Some(cont) => {
                            match apply_cont(cont, result_ty, state, errors, type_map, stack).await {
                                TypeCheckAction::Done(t) => {
                                    result_ty = t;
                                    // Keep draining — apply_cont returned Done, pop the next cont.
                                }
                                TypeCheckAction::Eval(next_node, next_env) => {
                                    current_node = next_node;
                                    current_env = next_env;
                                    break; // Back to outer loop for infer_step.
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

There are two loops. When `infer_step` returns `Eval`, the outer loop immediately calls `infer_step` again on the new node. When `infer_step` returns `Done`, an inner loop drains the continuation stack by repeatedly calling `apply_cont` until either the stack is empty (return the final type) or `apply_cont` returns `Eval` (break back to the outer loop for `infer_step`). A single `apply_cont` returning `Done` does not re-enter `infer_step` — the inner loop keeps popping and applying continuations until it gets an `Eval` or exhausts the stack.

Both `infer_step` and `apply_cont` are `async fn`. External async operations (`unify`, `resolve_annotation`, `evaluate_resolver`) are awaited directly inline — no special continuation variant exists for them. The CEK loop eliminates recursive calls to `run_typecheck`, not all async behavior.

---

## TypeCheckCont Variants

`TypeCheckCont` is the defunctionalized continuation enum. Each variant stores exactly the data needed to resume inference after a child expression has been processed. There are 14 variants.

### AfterFnBody

**Pushed by:** the `Fn { params, body }` arm of `infer_step`.

**Carries:** `saved_level` (the level to restore), `saved_expected_return` (the enclosing function's expected return type to restore), `return_ann` (the resolved return type annotation, if any), `params` (the list of `(Option<String>, Type)` pairs already built from the parameter list), `is_variadic`, `required_count`, `node_span`.

**What `apply_cont` does:** receives the body type from the child. Restores `state.level` and `state.expected_return` to the saved values. Constructs `Type::Function { params, ret: Box::new(child_ty), variadic, required_count }`. Returns `Done(fn_type)`.

### AfterCallFunc

**Pushed by:** the `Call { func, args, named_args }` arm of `infer_step`.

**Carries:** `args` (positional argument nodes to check), `named_args` (named argument entries), `env` (the call site environment), `span` (the call span for error messages), `call_node` (the full call node, for OnceLock writes).

**What `apply_cont` does:** receives the inferred function type. Instantiates the scheme if polymorphic. If there are positional arguments, pushes `AfterCallArg` for the first argument and returns `Eval(args[0], env)`. If there are no arguments, performs arity checking and named-argument unification directly, then returns `Done(return_type)`.

### AfterCallArg

**Pushed by:** `AfterCallFunc` (for the first argument) and by itself (for each subsequent argument).

**Carries:** `idx` (index of the argument just evaluated), `remaining_args` (nodes still to process), `accumulated_arg_types` (types already inferred, for error messages), `arg_nodes` (all positional arg nodes — used for gradual typing `type_guard` writes), `param_types` (the function's full parameter list), `fn_ret` (the function return type), `fn_variadic`, `fn_required`, `env`, `named_args`, `span`, `call_node`.

**What `apply_cont` does:** receives one argument type. Unifies it with `param_types[idx]` using Robinson unification (`state.unify()`). If `remaining_args` is non-empty, pushes another `AfterCallArg` (incrementing `idx`) and returns `Eval(remaining_args[0], env)`. When all positional arguments are processed, handles named arguments inline (each named arg is unified with its corresponding parameter by name), checks arity against `fn_required`, and returns `Done(fn_ret)`.

This is the **single canonical call-checking path** — see [Call-Checking Unification](#call-checking-unification) below.

### AfterMatchScrutinee

**Pushed by:** the `Match { scrutinee, arms }` arm of `infer_step`.

**Carries:** `arms` (the full arm list), `env` (the match site environment), `span`.

**What `apply_cont` does:** receives the scrutinee type. Runs exhaustiveness checking upfront on all arms via `run_match_exhaustiveness_check`. Then calls `setup_match_arm_env` to build the first arm's environment (pattern bindings, guard inference, narrowing). Pushes `AfterMatchArm` for the first arm and returns `Eval(arms[0].body_expr(), arm_env)`.

### AfterMatchArm

**Pushed by:** `AfterMatchScrutinee` (for the first arm body) and by itself (for each subsequent arm body).

**Carries:** `remaining_arms` (arms still to process), `env` (the original match environment, used to derive narrowing for each arm), `accumulated_types` (body types collected so far), `scrutinee_ty`, `remaining_scrutinee` (remaining type coverage for I-Case3 narrowing — updated per arm), `span`.

**What `apply_cont` does:** receives one arm body type. Appends it to `accumulated_types`. If `remaining_arms` is non-empty, calls `setup_match_arm_env` for the next arm and pushes another `AfterMatchArm`, returning `Eval(remaining_arms[0].body_expr(), arm_env)`. When all arms are processed, computes the union of `accumulated_types` and returns `Done(union_type)`.

Note: guard inference for each arm still calls `infer_surface_expr` internally (via `setup_match_arm_env`). Only arm body inference is fully iterative via the `AfterMatchArm` chain.

### AfterDictSccMember

**Defined in:** `typecheck_cek.rs`. Carries SCC state across member inferences.

**Current status:** Pushed by `AfterDictPassZero` for each SCC member. The `AfterDictPassZero` handler calls `run_typecheck_dict` which manages the full SCC loop. `AfterDictSccMember` carries the per-SCC state across member inferences within that loop.

**Intended behavior (when T-1644 completes):** pushed by `AfterDictPassZero` for the first SCC member and self-re-pushes for each subsequent member. When an SCC is complete, performs generalization and extends `dict_env`. When all SCCs are processed, returns `Done(dict_type)`.

### AfterDictPassZero

**Pushed by:** the `Dict { entries }` arm of `infer_step`. The Dict arm runs Pass 0 (key name resolution) synchronously, then pushes `AfterDictPassZero` and returns `Done(Unknown)` to immediately trigger `apply_cont`.

**Carries:** `dict_node` (the original Dict AST node for delegation), `entries`, `key_entries`, `env`, `enclosing_level`, `span`.

**What `apply_cont` does:** calls `run_typecheck_dict(entries, env, state, type_map, span)` which runs the full multi-pass SCC-based dict inference. Returns `Done(dict_type)`.

**Intended behavior (when T-1644 completes):** runs `compute_sccs()`, allocates fresh TypeVars for all entries, performs type alias registration (Pass 2), performs class/instance pre-registration (Pass 0c), pushes `AfterDictSccMember` for the first SCC member, and returns `Eval(first_member.value, scc_env)`.

### AfterTypeAliasReg

**Status:** handler exists (returns `Done(child_ty)`). Type alias registration is handled inside `run_typecheck_dict` during Pass 2 (synchronously). This continuation is reserved for future incremental dict CEK migration.

**Intended behavior (when T-1644 completes):** pushed by `AfterDictPassZero` to perform Pass 2 (type alias body resolution and TyConDef registration). When all aliases are registered, pushes `AfterClassInstancePreReg`.

### AfterClassInstancePreReg

**Current status:** defined and has a handler (returns `Done(child_ty)`), but is not currently pushed. **Currently unreachable.**

**Intended behavior (when T-1644 completes):** pushed by `AfterTypeAliasReg` after all type aliases are registered. Performs class and instance pre-registration (injecting method stubs into `dict_env`). After pre-registration, pushes `AfterDictSccMember` for the first SCC member.

### AfterSequentialExpr

**Pushed by:** the `Sequential(exprs)` arm of `infer_step`.

**Carries:** `remaining` (expression nodes not yet processed), `env` (the environment to use for each step).

**What `apply_cont` does:** receives the type of one expression. If `remaining` is non-empty, pushes another `AfterSequentialExpr` and returns `Eval(remaining[0], env)`. When the last expression completes, returns `Done(ty)` — the type of the final expression is the type of the entire sequential. The handler does not extend `env` with intermediate dict bindings; that extension is performed by the `Sequential` arm in `infer_surface_expr` (typecheck.rs) and will remain there until T-1644 wires the full dict CEK path.

### AfterTypeAssertInner

**Pushed by:** the `TypeAssert { expr, annotation }` arm of `infer_step`, after resolving `annotation` synchronously (via `resolve_annotation`).

**Carries:** `expected` (the resolved annotation type), `default` (the optional default expression node), `has_default`, `env`, `span`, `annotation_span`.

**What `apply_cont` does:** receives the inner expression type. Unifies `inner_ty` with `expected`. On mismatch without a default, records a type error and returns `Done(expected)` — the annotation type is returned so downstream uses see the declared type rather than the incorrect inferred one. On mismatch with a default (`has_default`), suppresses the error and type-checks the default node inline via `Box::pin(run_typecheck(...))` with a fresh local stack — this validates the default's type but does not push a continuation or return `Eval`. Returns `Done(expected)` in all cases (success, mismatch-without-default, and mismatch-with-default). Does not write `TypeAnnotation` OnceLocks or `TypeAnnotationTable` entries.

---

## Call-Checking Unification

Before the CEK machine, call type checking had three separate code paths that had to be kept in sync:

1. **Inline poly approximation** in `infer_surface_expr`'s Call arm (`typecheck.rs:1746–1929`): handled `VarRef`-with-scheme calls inline using direct `state.subst.type_map.borrow_mut().insert()` for TypeVar binding. This was a parallel re-implementation of path 2, with subtle differences — it used `is_consistent_subtype` for conflict checking rather than Robinson unification, producing different results for the same inputs.

2. **`check_call_with_scheme`** (`typecheck_call.rs:319–493`): the principled path for polymorphic calls. Used scheme instantiation followed by `check_call_args` delegation.

3. **`check_call`** (`typecheck_call.rs:1093–1339`): the general case — infers the function expression, then dispatches to `check_call_with_scheme` (CALL-POLY) or `check_call_args` directly (CALL-MONO).

The CEK machine consolidates all three into a single path through `AfterCallFunc` and `AfterCallArg`:

- `AfterCallFunc` receives the already-inferred function type (from whatever source — VarRef scheme, dict field, lambda, etc.) and begins argument processing.
- `AfterCallArg` calls `state.unify(arg_type, param_type)` (Robinson unification) for every argument. There is no `is_consistent_subtype` fast path. There is no `CALL-MONO`/`CALL-POLY` dispatch — unification handles both cases uniformly.
- Named arguments are handled after all positional arguments by iterating `named_args` and unifying each with the corresponding parameter by name.

This eliminates the soundness difference between the old paths (direct `subst.insert` vs. `unify`) and removes ~600 lines of parallel call-checking code.

---

## Dict Inference

Dict type inference is multi-pass. The CEK machine encodes this as a continuation chain:

```
infer_step(Dict)
    → resolve key names synchronously
    → push AfterDictPassZero
    → Eval(sentinel to trigger pass transition)

apply_cont(AfterDictPassZero)
    → compute_sccs() [Tarjan — synchronous]
    → allocate fresh TypeVars for all entries
    → push AfterTypeAliasReg if type aliases present
    → push AfterClassInstancePreReg after aliases
    → push first AfterDictSccMember
    → Eval(first_entry.value, scc_env)

apply_cont(AfterDictSccMember, member_ty)
    → unify member_ty with fresh TypeVar
    → if more members in SCC: push AfterDictSccMember, Eval(next)
    → if SCC complete:
        → generalize (skip for recursive SCCs — value restriction)
        → extend dict_env with schemes
        → if more SCCs: push AfterDictSccMember for first member of next SCC
        → if all SCCs done: Done(dict_type)
```

The SCC computation (`compute_sccs`) runs synchronously during `AfterDictPassZero` — it is Tarjan's algorithm implemented iteratively (explicit work stack, no Rust recursion) so it is safe to run inside an async context without blocking. `collect_dependencies` does a worklist walk of each entry's value AST to identify references to sibling bindings.

Both `compute_sccs` and `collect_dependencies` have their canonical implementations in `src/typecheck_cek.rs`. `src/typecheck_dict.rs` retains private delegation shims (`type_contains_typevar`, `adt_value_type`) and compute_sccs unit tests that call the canonical implementations directly.

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

The symmetry is deliberate. Both systems convert a recursive tree walk into a loop with an explicit heap-allocated stack. Both use a two-variant "ready/continue" enum to drive the loop. Both `apply_cont` functions receive the result of a child computation and decide what to do next.

The key structural difference is that the evaluator operates on `Thunk`s (lazy values with memoization) while the type checker operates on `SurfaceNode`s (AST nodes, not memoized). The evaluator's `Memoize` continuation has no counterpart in the type checker — type inference is a one-pass write, not a cached result.

---

## File Locations

| File | Role |
|---|---|
| `src/typecheck_cek.rs` | `TypeCheckCont`, `TypeCheckAction`, `run_typecheck`, `infer_step`, `apply_cont`, `compute_sccs` (canonical), `collect_dependencies`, `type_contains_typevar`, `adt_value_type`, `entry_key_name` |
| `src/typecheck.rs` | Top-level entry points (`typecheck_surface_program_annotation_table`, etc.); declares all typecheck submodules; contains `infer_surface_expr` (transitional — retained for Decl variants and as a bridge from CEK to dict inference) |
| `src/typecheck_dict.rs` | Private delegation shims (`type_contains_typevar`, `adt_value_type`) and compute_sccs unit tests. `infer_dict` is deleted — `run_typecheck_dict` in `typecheck_cek.rs` is now the canonical dict-inference path. |
| `src/typecheck_call.rs` | Surviving helpers after S-930: `widen_literal_types`, `check_dot_access`, `check_dot_access_int`, `is_concrete_type` — call-checking functions absorbed into `AfterCallFunc`/`AfterCallArg` |
| `src/typecheck_annot.rs` | Annotation resolution: `resolve_annotation` (called from `infer_step` for `TypeAssert` and parameter annotations) |
| `src/type_normalize.rs` | `evaluate_resolver` — async function for resolving parameterized type-stage types; awaited inline from `infer_step` |
