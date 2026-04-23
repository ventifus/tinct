# Implementation Roadmap

Extracted from DESIGN.md. Tracks what's next and what's deferred. Completed work is in DONE.md.

## error-structured: Structured Error Model Implementation

Implement the `ErrorKind` enum and migrate all error construction sites. See DESIGN.md §Structured Error Model.

### error-structured-types: ErrorKind Type Definitions

Define the structured error types and update EvalError to use them.

- [x] Add `ErrorKind` enum with 25 variants and `ArityBound` enum to `src/error.rs`
- [x] Add `ErrorKind::code()` method returning stable error code strings (`src/error.rs`)
- [x] Add `ErrorKind::is_cacheable()` method returning `false` for `DepthExceeded` (`src/error.rs`)
- [x] Add `Display` impl for `ErrorKind` and `ArityBound` (`src/error.rs`)
- [x] Replace `message: String` with `kind: ErrorKind` in `EvalError` struct (`src/error.rs:20-25`)
- [x] Update `EvalError::Display` to include error code prefix `[E001]` (`src/error.rs:85-95`)
- [x] Update named constructors (`key_not_found`, `type_mismatch`, `arity_mismatch`, `circular_dependency`) to construct `ErrorKind` variants (`src/error.rs:59-82`)
- [x] Add `EvalError::internal(message, span)` replacing `EvalError::new` — construct `ErrorKind::Internal` (`src/error.rs:28-35`)

### error-structured-migrate-a: Priority Semantic Fixes

Fix error classification bugs where the wrong ErrorKind variant is produced.

- [x] Migrate `$error` builtin to use `ErrorKind::UserError` instead of `EvalError::new` (→ Internal E099) — `$error` is the canonical user error source; every user-generated error displays `[E099]` (internal) instead of `[E080]` (user). Add `EvalError::user_error(msg, span)` constructor. (`src/builtins.rs:791`) [Major, computer-scientist]
- [x] Migrate `eval()` depth check to use `EvalError::depth_exceeded()` — currently `EvalError::new()` → Internal (E099), inconsistent with `materialize()` which correctly uses `depth_exceeded()`. Breaks `is_cacheable()` invariant when integrated. (`src/eval.rs:59-64`) [Major, computer-scientist + eval-engine]
- [x] Migrate `deep_materialize_impl()` depth check to use `EvalError::depth_exceeded()` — same issue as `eval()` depth check (`src/eval.rs`) [Minor, eval-engine]
- [x] Generalize `FloatNotFinite` Display message — currently says "cannot be converted to Int" but variant covers all non-finite contexts; change to context-independent message like "{builtin}: result is not finite ({value})" (`src/error.rs:228-230`) [Minor, span-integrity-checker + computer-scientist]
- [x] Migrate `require_dict` and `require_string` helpers to use `EvalError::type_mismatch()` — currently use `EvalError::new(format!(...))` → Internal, losing structured error classification (`src/builtins.rs:165-169,177-181`) [Minor, span-integrity-checker]
- [x] Migrate `reject_named` to use `ErrorKind::NamedArgRejected` variant — currently `EvalError::new(format!(...))` → Internal (`src/builtins.rs:192-194`) [Nit, span-integrity-checker + integration-verifier]
- [x] Migrate `checked_f64_to_i64` to use `ErrorKind::FloatNotFinite` — currently `EvalError::new(format!(...))` → Internal (`src/builtins.rs:108-109`) [Nit, span-integrity-checker]

### error-structured-migrate-b: Bulk Migration

Migrate remaining EvalError::new call sites across eval.rs and builtins.rs.

- [x] Migrate eval.rs remaining `EvalError::new` call sites (~13) to typed `ErrorKind` variants (`src/eval.rs`)
- [x] Migrate builtins.rs remaining `EvalError::new` call sites (~72) to typed `ErrorKind` variants (`src/builtins.rs`)
- [x] Update `builtin_try` to extract `e.kind.to_string()` instead of `e.message` (`src/builtins.rs:864`)
- [x] Update all `err.message` references in unit tests to `err.kind.to_string()` or pattern matching (`src/error.rs`, `src/eval.rs`)
- [x] Add PROP-CYCLE bypass comment to circular dependency error construction — explain why it skips DECORATE closure. (`src/eval.rs:899-908`) [Nit, eval-engine panel]
- [x] Document `.message()` as compatibility shim — clarify that `.kind` field is canonical API, `.message()` is for test migration. New code should match on `.kind`. (`src/error.rs`) [Minor, integration-verifier]

### error-structured-migrate-c: Safety Integration + Spec

Error safety mechanisms and specification updates.

- [x] Add `ErrorKind::is_catchable()` method returning `false` for `DepthExceeded` — `$try` currently catches ALL errors including depth-exceeded, defeating the safety net. Users can circumvent depth limits via `$until` + `$try` wrapping deeply recursive code. GHC makes `StackOverflow` uncatchable; Racket separates `exn:fail:resource`. Have `builtin_try` check `is_catchable()` and re-raise uncatchable errors directly. (`src/builtins.rs:793-871`, `src/error.rs`) [Critical, computer-scientist]
- [x] Integrate `is_cacheable()` into `cache_failure` — currently `Thunk::cache_failure()` unconditionally caches all errors including DepthExceeded. Requires state-restore mechanism: save pre-InProgress thunk state so non-cacheable errors can restore it instead of transitioning to Failed. (`src/value.rs:384-386`) [Major, eval-engine + computer-scientist panel]
- [x] Fix `FloatNotFinite` containing `f64` with `PartialEq` derive — `f64::NAN != f64::NAN` so two FloatNotFinite errors with NaN values compare as not-equal. Will affect Failed thunk cache identity when variant is constructed. (`src/error.rs`) [Minor, computer-scientist panel]
- [x] Update SPEC.md §9 with `ErrorKind` variants, error codes in display format (§9.2), and revised exhaustiveness claim in §9.3
- [x] Update DESIGN.md §Error Semantics field name: `message` → `kind` — spec still references old field name (`DESIGN.md`) [Minor, eval-engine]
- [x] Update DESIGN.md error constructors table to reflect ErrorKind variants — either expand or add "representative" note (`DESIGN.md`) [Minor, eval-engine]

### error-structured-migrate-c2: Formal Rule Drift Fixes

DESIGN.md inference rules don't reflect is_cacheable()/is_catchable() guards added in migrate-c.

- [x] Update PROP-EVAL/PROP-BUILTIN/PROP-RESULT inference rules to add `is_cacheable()` precondition on Failed transition — rules show unconditional `thunk.state <- Failed(e')` but implementation conditionally restores pre-error state for non-cacheable errors. Add alternative conclusion showing state restoration when `!is_cacheable()`. (`DESIGN.md:4584-4617`) [Critical, computer-scientist]
- [x] Add TRY-UNCATCHABLE rule and is_catchable() precondition to TRY-ERR — rule unconditionally converts Err(e) to err dict, but builtin_try now re-raises uncatchable errors (DepthExceeded). Add precondition `e.kind.is_catchable()` to TRY-ERR and new TRY-UNCATCHABLE rule showing re-raise. (`DESIGN.md:4690-4699`) [Critical, computer-scientist]
- [x] Update MEMO-CACHE rule to add `is_cacheable()` precondition — rule and prose say "All error paths cache via cache_failure" unconditionally. Add precondition and MEMO-SKIP rule for non-cacheable errors. (`DESIGN.md:4648-4655`) [Critical, computer-scientist]
- [x] Update Implementation Correspondence table line numbers after is_cacheable integration shifted eval.rs structure (`DESIGN.md:4745-4759`) [Minor, computer-scientist]
- [x] Fix E2 property `e.message` to `e.kind` — field was replaced by ErrorKind in error-structured-types (`DESIGN.md:4725`) [Minor, computer-scientist]
- [x] Add note to SPEC.md §9.4 that DepthExceeded errors are not catchable by $try — users may be surprised when $try doesn't catch resource limit errors (`SPEC.md:1436-1449`) [Minor, computer-scientist]

### error-structured-migrate-c3: Residual Doc Precision

Residual documentation fixes from c2 review: informal notation, incomplete parentheticals, code/spec alignment.

- [x] Update TRY-BUILTIN error parenthetical with is_catchable() qualifier — after c2 added TRY-UNCATCHABLE, the parenthetical "(Error variant: same structure, `Err(ε) ⇒ Dict({err ↦ ...})`)" doesn't mention catchability guard. Change to "(Catchable error variant: same structure; uncatchable errors re-raised per TRY-UNCATCHABLE)". (`DESIGN.md:4745`) [Minor, computer-scientist]
- [x] Fix Error-to-value correspondence to match actual code path — table says "extract `e.kind.to_string()`" but code uses `e.message()` which delegates to `e.kind.to_string()`. Either update table to say `e.message()` or change code to call `e.kind.to_string()` directly. (`DESIGN.md:4794`, `src/builtins.rs:877`) [Minor, computer-scientist]
- [x] Update PROP-DEPTH constructor notation to typed ErrorKind style — rule says `ε = new("maximum evaluation depth exceeded", thunk.span)` but implementation uses `EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk.span)`. Change to `ε = depth_exceeded(MAX_EVAL_DEPTH, thunk.span)`. (`DESIGN.md:4647`) [Nit, computer-scientist]

### error-structured-migrate-d: Test Coverage

ErrorKind test coverage to validate migration and prevent regressions.

- [x] Add missing ErrorKind constructor methods for remaining ~13 variants — DuplicateKey, NamedArgConflict, UnknownNamedArg, ParseConversion, TypeAssertFailed, UndefinedVariable, all JSON/Include variants still use verbose `Box::new(EvalError { kind: ..., ... })` instead of named constructors. Add constructors and migrate call sites. (`src/error.rs`, `src/eval.rs`, `src/builtins.rs`) [Minor, computer-scientist + eval-engine]
- [x] Add ErrorKind Display unit tests for all 25 variants — only 4 of 25 tested (KeyNotFound, TypeMismatch, ArityMismatch, CircularDependency). (`src/error.rs`) [Major, test-crafter panel]
- [x] Add ArityBound Display unit tests — no isolated tests for Exact/AtMost/Range Display impls. (`src/error.rs`) [Minor, test-crafter panel]
- [x] Add is_cacheable() unit tests — verify DepthExceeded returns false, all others true. (`src/error.rs`) [Minor, test-crafter panel]
- [x] Add error code prefix verification to corpus error tests — substring matching doesn't check for `[E0XX]` prefix. (`tests/corpus/eval/errors/`) [Minor, test-crafter panel]
- [x] Add `ErrorKind::code()` exhaustiveness unit test — assert all variants return "E" + digits, prevents silent breakage when new variants added (`src/error.rs`) [Minor, test-crafter]
- [x] Add stack frame propagation integration tests — test multi-level error propagation through nested materialization chains (dict → thunk → builtin → error), verify frames accumulate correctly (`src/error.rs`) [Major, test-crafter]

## evalcontext-refactor: EvalContext Parameter Threading

Replace thread-local `INCLUDE_CTX` with parameter-passed `EvalContext`. Unlocks LSP multi-file support and clean sandboxing. See DESIGN.md §EvalContext.

**Unlocks:** `sandbox` (filesystem allowlist lives in EvalConfig)

- [ ] Create `EvalConfig` struct — `base_dir: PathBuf`, `stdlib_env: Rc<RefCell<Environment>>`, `allowed_paths: Vec<PathBuf>` (`src/eval.rs`)
- [ ] Create `EvalState` struct — `include_guard: HashSet<PathBuf>`, `include_cache: HashMap<PathBuf, Rc<Thunk>>` (`src/eval.rs`)
- [ ] Create `EvalContext` struct — `config: Rc<EvalConfig>`, `state: Rc<RefCell<EvalState>>` (`src/eval.rs`)
- [ ] Add `ctx: Rc<EvalContext>` field to `ThunkState::Unevaluated`, `PendingBuiltin`, `PendingCall` (`src/value.rs:176-190`)
- [ ] Add `ctx: Rc<EvalContext>` field to `BuiltinArgs` (`src/builtins.rs`)
- [ ] Thread `EvalContext` through `eval()`, `materialize()`, and builtin dispatch (`src/eval.rs`)
- [ ] Migrate `builtin_include` to use `ctx.state` instead of thread-local `INCLUDE_CTX` (`src/builtins.rs:1024-1170`)
- [ ] Remove thread-local `INCLUDE_CTX` and `set_include_context`/`clear_include_context` (`src/builtins.rs:58-70`, `src/lib.rs`)
- [ ] Update CLI (`main.rs`): construct `EvalContext` from file path
- [ ] Update public API: `EvalContext`, `EvalConfig`, `EvalState` are public; remove `set_include_context`/`clear_include_context`
- [ ] Update all include-related tests

## let-generalization: Levels-Based Let-Generalization

Implement proper Hindley-Milner let-polymorphism with levels-based generalization (Kiselyov 2013). Without this, polymorphism requires explicit annotations. See DESIGN.md §Let-Generalization (Levels-Based).

- [ ] Add `TypeScheme` struct — `vars: Vec<String>`, `body: Type`, `TypeScheme::mono(ty)` constructor (`src/types.rs`)
- [ ] Add `InferState` struct — `name_counter: u32`, `level: u32`, `levels: HashMap<String, u32>` (`src/types.rs`)
- [ ] Change `Type::TypeVar(String)` to `TypeVar(String, u32)` with manual `PartialEq` on name only (`src/types.rs:36`)
- [ ] Change `RowRest::RowVar(String)` to `RowVar(String, u32)` with level (`src/types.rs`)
- [ ] Change `TypeEnv.bindings` from `IndexMap<String, Type>` to `IndexMap<String, TypeScheme>` (`src/types.rs`)
- [ ] Replace `counter: &Cell<u32>` parameter with `state: &mut InferState` in `infer_expr`/`infer_dict`/`infer_fn` (`src/typecheck.rs`)
- [ ] Implement `instantiate_scheme(scheme, state) -> Type` — freshen all vars at current level (`src/types.rs`)
- [ ] Implement `generalize(level, ty) -> TypeScheme` — collect vars with level > given, abstract them (`src/types.rs`)
- [ ] Update VAR rule: `instantiate_scheme(env.get(name)?, state)` (`src/typecheck.rs`)
- [ ] Update `infer_dict` to 5 passes: key resolution, bind-all (fresh α at `level+1`), type aliases, infer values (at `level+1`), generalize (`src/typecheck.rs`)
- [ ] Implement symmetric level lowering in unify U-VAR rules (`src/types.rs`)
- [ ] Implement Any-unification level zeroing: `unify(α, Any)` sets `level(α) = 0` (`src/types.rs`)
- [ ] Update `typecheck_document` to thread `TypeScheme`s across `---` boundaries (`src/typecheck.rs`)
- [ ] Update all `TypeVar("a".into())` in tests to `TypeVar("a".into(), 0)` (`src/types.rs`, `src/typecheck.rs`)
- [ ] Fix letrec forward-reference typing to Any — resolved by 5-pass bind-all approach (`src/typecheck.rs:225`) [Minor, computer-scientist]
- [ ] Add tests: polymorphic identity generalizes, nested dicts increment levels, Any-touched vars not generalized

## bidirectional-typing: Bidirectional Type Checking

Implement bidirectional type checking with synthesis and checking modes. See DESIGN.md §Bidirectional Typing.

**Benefits from:** `let-generalization` (uses `InferState` parameter)

- [ ] Implement `check_expr(expr, expected, env, state, type_map) -> Result<(), Vec<TypeError>>` (`src/typecheck.rs`)
- [ ] Apply `check_expr` at CALL-MONO argument positions (expected type fully concrete) (`src/typecheck.rs`)
- [ ] Apply `check_expr` for function body with return annotation (`src/typecheck.rs`)
- [ ] Apply `check_expr` for `TypeAssert` inner expression (`src/typecheck.rs`)
- [ ] Keep `unify` + U-SUBSUME for CALL-POLY argument positions (type variables present) (`src/typecheck.rs`)
- [ ] Fix function variance inconsistency between `unify` and `is_subtype` — `check_expr` applies [SUB] at leaves, resolving the dual-path divergence (`src/types.rs:291-315, 67-82`) [Major, computer-scientist]
- [ ] Implement lambda checking mode — `infer_fn` checks against expected function type. Pierce & Turner (2000), Dunfield & Krishnaswami (2021). (`src/typecheck.rs:449`) [Major, computer-scientist]
- [ ] Implement checking mode propagation to call arguments — Pierce-Turner S-App rule. (`src/typecheck.rs:389-446`) [Major, computer-scientist]
- [ ] Fix monomorphic function calls skipping argument type checking — `!func_ty.has_type_vars()` bypasses argument-parameter unification (`src/typecheck.rs:421-422`) [Major, computer-scientist]
- [ ] Implement constraint-based type argument synthesis for polymorphic calls — Pierce-Turner constraint generation with variance-aware minimal substitution (`src/typecheck.rs:425-439`) [Minor, computer-scientist]
- [ ] Add tests: literal promotion via subsumption, lambda parameter inference from context

## call-convention-kotlin: Kotlin-Model Call Convention

Replace count-based arity with per-parameter coverage check. Allow named args for any parameter. See DESIGN.md §Call Convention — Formal Specification.

- [ ] Implement C-COVERAGE: per-parameter coverage check replacing `positional.len() < required_count` (`src/eval.rs:520-626`) [Major, eval-engine]
- [ ] Allow named args for any parameter — remove `get_default(p).is_some()` guard (`src/eval.rs`)
- [ ] Implement Garrigue default-env separation — `env_d` (definitions) vs `env_c` (call-site) (`src/eval.rs`)
- [ ] Implement 4 error classes: E-COVERAGE, E-CONFLICT, E-UNKNOWN, E-EXCESS (`src/eval.rs`)
- [ ] Support named args from dict in `$apply` — integer-keyed → positional, string-keyed → named. Garrigue (1995). (`src/builtins.rs:878-932`) [Minor, eval-engine]
- [ ] Add tests for each binding constraint and error class
- [ ] Add tests for interleaved required/optional parameters

## typeassert-structural: TypeAssert Structural Contract Checking

Replace nominal type tag checking with structural contract validation. See DESIGN.md §TypeAssert Runtime Validation.

**Depends on:** `bidirectional-typing` for `check_expr` in elaboration flow

- [ ] Add `resolved_type: Option<Type>` field to `Expr::TypeAssert` (`src/ast.rs`)
- [ ] Update `resolve_type_assert()` to set `resolved_type` on AST node (elaboration) (`src/typecheck.rs:503-523`)
- [ ] Implement `value_matches_type(value, type, span) -> Result<bool, EvalError>` for immediate validation (`src/eval.rs`)
- [ ] Add `ThunkState::Guarded { inner, expected, field_path, guard_span }` variant (`src/value.rs`)
- [ ] Implement proxy contract wrapping: shape check + guard wrapping for record field thunks. Findler & Felleisen (2002). (`src/eval.rs:117-157`)
- [ ] Implement guard memoization: `Guarded` → `Materialized` or `Failed` after first force (`src/eval.rs`)
- [ ] Handle `--no-typecheck` fallback — degrade to current nominal behavior when `resolved_type` is `None` (`src/eval.rs`)
- [ ] Implement blame tracking with even-odd polarity for contract positions. Findler & Felleisen (2002). (`src/eval.rs`) [Major, computer-scientist]
- [ ] Use chaperone semantics for record proxies. Strickland et al. (2012). (`src/eval.rs`) [Minor, computer-scientist]
- [ ] Close elaboration gap — evaluator must enforce structural types for eval-only mode soundness. (`src/typecheck.rs:503-523`, `src/eval.rs:117-157`) [Major, computer-scientist]
- [ ] Add tests for each validation rule and proxy/guard lifecycle

## merge-lazy-overlay: Lazy Dict Overlay for $merge

Replace eager dict merge with lazy overlay representation. See DESIGN.md §Selective Materialization.

- [ ] Implement `Overlay(L, R)` representation for `Value::Dict` — O(1) construction without materializing L or R
- [ ] Access semantics: check R first, then L
- [ ] Iteration: flatten to concrete `IndexMap` on demand
- [ ] Handle chained overlays: `Overlay(Overlay(A, B), C)`
- [ ] Verify behavioral equivalence: same values, same iteration order, same errors, same sharing
- [ ] Benchmark: compare eager merge vs lazy overlay on large dicts

## Sequences and Fully Lazy Operations (remaining)

### seq-cycle-fix: Seq Cycle Detection Asymmetry

Found by eval-engine codebase review (2026-04-20).

**Design decision (2026-04-21):** Pointer-identity visited set (`HashSet<*const Thunk>`) + depth/fuel backstop is sufficient. Cross-type cycles through distinct thunk allocations are nearly impossible to construct in tinct's Rc-sharing model (letrec uses `Rc::clone`, not copy). CEK migration replaces depth limit with explicit fuel, which serves the same backstop role. No value-level allocation tagging needed.

- [x] Design value-level vs pointer-level cycle detection strategy for `deep_materialize` — decision: pointer identity + depth/fuel backstop is sufficient; cross-type cycles through distinct allocations are nearly impossible in Rc-sharing model. No changes needed; CEK migration (iterative `Cont::DeepEntries`/`Cont::DeepSeqTail`) subsumes the stack overflow concern. (`src/eval.rs:1109-1139`) [Major, eval-engine]
- [ ] Document deep_materialize Seq→Dict terminal case — docstring doesn't explain that Seq tail eventually materializes to empty Dict `[]` as terminal value, and infinite sequences hit MAX_EVAL_DEPTH (`src/eval.rs:1056-1069`) [Minor, eval-engine]
- [ ] Fix deep_materialize breaking Launchbury sharing — creates new `Rc<Thunk>` allocations for every Dict entry/Seq element, so two references that shared the same thunk via `Rc::clone` will have `Rc::ptr_eq` return false after deep_materialize. Only affects `--eval` output path. Fix: maintain `HashMap<*const Thunk, Rc<Thunk>>` during traversal to reuse forced replacements. (`src/eval.rs:1090-1107`) [Minor, computer-scientist]

### seq-resource-safety: Sequence Resource Safety

Resource safety gaps in sequence combinators. Found by computer-scientist codebase review (2026-04-22).

- [x] Add `MAX_COLLECT_SIZE` limit to `builtin_collect` — iterates Seq spine in a Rust `loop` with no iteration count limit; `depth` parameter never incremented inside the loop (iterative, not recursive on Rust call stack), so `MAX_EVAL_DEPTH` never fires. `[call $collect [call $range 0]]` loops until memory exhaustion. DoS vector for user-supplied tinct code. Add limit (e.g., 1,000,000) and suggest `$take` before `$collect` in error message. (`src/builtins.rs:1275-1302`) [Critical, computer-scientist]
- [x] Fix `builtin_iterate` passing `depth: 0` to PendingBuiltin tail — `BuiltinArgs` destructuring uses `..` to ignore `depth`, then hardcodes `0` for the recursive tail thunk. Resets depth counter on every step, so depth backstop never fires for `iterate` chains. Compare `builtin_unfold_step` which correctly passes caller's `depth`. One-line fix. (`src/builtins.rs:1555-1592`) [Major, computer-scientist]
- [x] Increment depth in sequence combinator PendingBuiltin chains — `range`, `unfold_step`, `drop_seq_step`, `reduce_seq_step` all create recursive PendingBuiltin chains storing the same `depth` value (never incrementing). A `$filter` examining millions of elements before finding a match will not be caught by `MAX_EVAL_DEPTH`. PendingBuiltin chains are defunctionalized continuations (Reynolds 1972); in a CEK machine each continuation push would consume one unit of fuel. Either increment depth per step, add separate `steps` counter, or document that sequence combinators rely on `$take`/`MAX_COLLECT_SIZE` instead of depth limits. (`src/builtins.rs:1363-1370, 1644-1651, 2238-2260, 2351-2383`) [Major, computer-scientist]
- [x] Migrate `concat` Seq path from stdlib to Rust builtin (correctness, not just perf) — `stdlib/prelude.llt:303-308` implements `concat` for Seq via recursive user function call (`[call $seq [call $head $xs] [call $concat [call $tail $xs] $ys]]`); each step of left sequence consumes one `MAX_EVAL_DEPTH` level, so sequences > ~256 elements error. Without TCO (Clinger 1998), recursive cons-list operations consume stack proportional to list length. Implement as Rust builtin using PendingBuiltin chain (matching `map`/`filter` pattern). (`stdlib/prelude.llt:303-308`) [Major, computer-scientist]
- [ ] Add type validation to concat empty-xs path — `builtin_concat` returns `ys_thunk` directly when xs is empty Dict without checking ys type; `concat([], 42)` succeeds incorrectly. Add materialize+match guard. (`src/builtins.rs`) [Minor, computer-scientist + eval-engine panel]
- [ ] Add `checked_add` to concat Dict path index arithmetic — `idx += 1` is unchecked, inconsistent with `builtin_collect` and `builtin_append` which use `checked_add`. Overflow unreachable in practice but violates codebase convention. (`src/builtins.rs`) [Nit, eval-engine + performance-expert panel]
- [ ] Fix `$take` PendingBuiltin depth to use `depth + 1` — take doesn't increment depth in its chain, creating a depth-reset interposition layer. `$take 500 [call $range 0]` fails at ~257 elements because range's depth accumulates but take's doesn't. Practical sequence length limit of N < MAX_EVAL_DEPTH for composed pipelines is undocumented. Self-terminating (bounded by n) so depth tracking is redundant for take itself but constrains composed pipelines. (`src/builtins.rs:2166`) [Minor, computer-scientist panel]
- [ ] Fix `$filter` Seq initial PB depth inconsistency — filter Seq initial PB uses `depth + 1` but filter Dict initial PB uses `depth`. Other combinators (drop, reduce, unfold) consistently use `depth` for initial deferral and `depth + 1` for recursive steps. (`src/builtins.rs:1844,1857`) [Nit, computer-scientist panel]
- [ ] Correct TODO.md PendingBuiltin/CEK fuel correspondence description — depth in PendingBuiltin chains is an indirect stack-depth proxy that fires when builtins call `materialize`, not a true fuel counter (Sestoft 1997). In a CEK machine, continuation stack is checked on every transition; here, depth is checked only on `materialize` entry. The true resource safety for `$collect` comes from `MAX_COLLECT_SIZE`, not depth. Both mechanisms are complementary. [Minor, computer-scientist panel]
- [ ] Add type validation to concat_seq_step terminal case — when xs_tail materializes to Dict (sequence terminator), `ys_thunk` is returned directly without type checking; `concat(seq(1, 2, 3), 42)` defers the type error until consumer forces past last element. Distinct from empty-xs Dict path (line 157). Either eagerly validate ys or document intentional deferral. (`src/builtins.rs:2598-2600`) [Minor, computer-scientist]
- [ ] Fix concat/collect error paths to use operand span as definition-site — 4 error paths in `builtin_concat` (Dict ys type mismatch, initial xs type mismatch, step tail type mismatch) and `builtin_collect` (tail type mismatch) use `call_span` as definition-site instead of `args[N].span`. Should use `EvalError::type_mismatch(..., operand_span).with_materialization_span(call_span)`. (`src/builtins.rs:2565-2579, 2616-2623, 2305-2313`) [Major, span-integrity-checker]
- [ ] Resolve concat_seq_step depth increment inconsistency — uses `depth + 1` (line 2609) where other step functions (filter_seq_step, drop_seq_step, reduce_seq_step) use `depth` for the recursive PendingBuiltin. Either change to `depth` for consistency or document why concat needs `depth + 1`. (`src/builtins.rs:2609`) [Minor, eval-engine]

### float-nan-infinity: Float NaN/Infinity Propagation

Float arithmetic can silently produce NaN or Infinity values that propagate through the evaluator unchecked. Only caught at JSON serialization, far from the cause. Found by computer-scientist codebase review (2026-04-20).

- [x] Decide NaN/Infinity rejection policy — Option B: reject at both arithmetic result sites AND `$from-json` entry. "All floats are finite" invariant. Consistent with `$to-float`, matches Jsonnet/Nickel/CUE consensus for config languages targeting JSON output. See DESIGN.md §Equality and Comparison Part 5
- [ ] Add NaN/Infinity result check to `builtin_add` Float path — `a + b` can produce Infinity (`1e308 + 1e308`), reject at point of origin (`src/builtins.rs:210`) [Major, computer-scientist]
- [ ] Add NaN/Infinity result check to `builtin_sub` Float path — `a - b` can produce Infinity, and `-Inf - (-Inf)` produces NaN (`src/builtins.rs:229`) [Major, computer-scientist]
- [ ] Add NaN/Infinity result check to `builtin_mul` Float path — `a * b` can produce Infinity (`1e308 * 2.0`) and `0.0 * Inf` produces NaN (`src/builtins.rs:248`) [Major, computer-scientist]
- [ ] Add NaN/Infinity result check to `builtin_div_float` Float path — `Inf / finite` produces Infinity, `Inf / Inf` and `0.0 / 0.0` produce NaN (only `b == 0.0` is checked) (`src/builtins.rs:270-274`) [Major, computer-scientist]
- [ ] Add shared `check_float_result(f64, &str, Span)` helper returning error on `is_nan()` or `is_infinite()` (`src/builtins.rs`)
- [ ] Reject NaN/Infinity in `$from-json` parse path — add `is_finite()` check after `as_f64()` in `json_to_value` Number arm (`src/builtins.rs` json_to_value)

## Parser Rewrite (E2)

Replace pest's recursive descent with a hand-written lexer + iterative parser using an explicit stack. The pest parser stays as a reference implementation for comparison until the new parser graduates.

**Goal:** Identical AST output from both parsers, selectable at parse time. Once the new parser passes the full test suite and matches pest output on all corpus files, it becomes the default and pest is removed.

### iterative-parser: Iterative parser (`src/parser2.rs`)

Explicit `Vec<StackFrame>` for bracket nesting. Atoms and access chains parsed without recursion.

- [ ] StackFrame enum: Dict, Call, Fn, TypeAlias, TypeAssert (one variant per bracket form)
- [ ] On `[`: push frame, determine form from first token (keyword detection)
- [ ] On `]`: pop frame, construct AST node
- [ ] Between brackets: parse atoms, access chains, annotations (all non-recursive)
- [ ] Add BracketAccess token to lexer for whitespace-sensitive bracket access detection — currently the lexer emits plain OpenBracket for both `$a[0]` (bracket access) and `$a [0]` (new expression); iterative parser needs lexer-level disambiguation matching pest's compound-atomic ($) rule for bracket_access_chain (`src/lexer.rs`) [Major, computer-scientist]
- [ ] MAX_DEPTH check on `stack.len()` (policy, not safety)
- [ ] Static constraints: duplicate keys, variadic rules
- [ ] Error messages with precise context ("expected value after `:`", "unclosed bracket at line 5")
- [ ] Document type alias entries returning empty dict at runtime — add comment explaining compile-time-only behavior (`src/eval.rs:182-185`) [Minor, integration-verifier]

### parser-integration: Integration

- [ ] `parse()` API accepts parser selection (enum or feature flag)
- [ ] Both parsers produce identical `Spanned<File>` output
- [ ] Comparison test: parse every corpus file with both parsers, assert AST equality
- [ ] Benchmark: compare parse time on large inputs

### parser-graduation: Graduation criteria

- [ ] Full test suite passes (all unit + corpus tests)
- [ ] AST output matches pest parser on every corpus file
- [ ] Error messages are equal or better quality
- [ ] No stack overflow on any nesting depth up to MAX_DEPTH

### parser-cleanup: Cleanup (post-graduation)

- [ ] Remove `pest` and `pest_derive` dependencies from Cargo.toml
- [ ] Remove `src/grammar.pest`
- [ ] Remove pest-specific code from `src/parser.rs`
- [ ] Remove pest-specific test code and helpers from `src/parser.rs`
- [ ] Rename `src/parser2.rs` to `src/parser.rs`
- [ ] Update CLAUDE.md, README.md, SPEC.md references (remove pest notation, update grammar description)
- [ ] Full pest removal audit: verify no remaining pest references in docs, tests, or comments

## Performance: Stdlib Rust Reimplementations

Nearly all accumulator-based stdlib functions are O(n^2) due to `merge`/`append` materializing and cloning the growing accumulator IndexMap on every iteration. Sort is O(n^2 log n) because `sort-merge` uses `cons` (O(n)) per element.

**Note:** The Sequences milestone addresses much of this by moving `map`, `filter`, `range`, `take`, `drop`, `reduce` to Rust builtins with lazy dispatch. The items below track remaining performance work not covered by that milestone.

Remaining Rust reimplementations (all currently in `stdlib/prelude.llt`):
- [ ] `rest`, `cons`, `conj`, `concat`, `reverse` -- list primitives, used by sort (O(n) each due to cloning). Note: `concat` Seq path is also a correctness issue (hits depth limit at ~256 elements), tracked separately in seq-resource-safety
- [ ] `sort`, `sort-by` / `sort-merge` -- single Rust builtin using Vec::sort_by would be O(n log n) (laziness-auditor review: $sort uses eager $cons per element)
- [ ] `zip`, `flatten`, `find-deep` -- recursive traversal or lazy seq versions for perf

## perf-foundations: Performance Foundations

Performance improvements identified by performance-expert review (2026-04-19) that don't depend on the Parser Rewrite.

- [x] Design allocation strategy (arena vs Rc, flat env vs chain) — see DESIGN.md §Allocation Strategy — Phased Approach
- [ ] Arena allocation for thunks/environments — reduce per-thunk Rc<RefCell<ThunkState>> overhead, improve cache locality
- [ ] Flat environment with slot indices — replace O(n) chain walk with O(1) slot lookup (requires compile-time slot assignment)
- [ ] String interning for dict keys and small strings — reduce allocation pressure, improve key comparison
- [ ] Path-compressed union-find for type substitutions — make Substitution::apply() O(α(n)) amortized instead of O(size)
- [ ] Dict literal fast-path in eval_dict — skip Unevaluated thunk for `Int | Float | Bool | Str` literals, create Materialized directly (Nix `maybeThunk` optimization, ~40-60% fewer thunk allocations for config-heavy files) [Major, eval-engine + performance-expert]
- [ ] Key cloning reduction in eval_dict — string keys cloned 2× per entry (once into dict_env bindings, once into dict_map). Use entry_mut() pattern or restructure insert order. ~30% of dict allocation cost. (`src/eval.rs:346-352`) [Major, performance-expert, design-review]
- [ ] func_label allocation reduction — `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for VarRef case (most common), only allocate for DotAccess. ~5-10% call overhead. (`src/eval.rs:387-396`) [Minor, performance-expert, design-review]
- [ ] Capacity hints for hot-path allocations — `IndexMap::with_capacity(entries.len())` in dict construction, `String::with_capacity()` in `builtin_str` [Minor, performance-expert]
- [ ] Reduce bind_args_thunks allocation — deferred to iterative-eval: env reuse is unsafe in current `Rc<RefCell<Environment>>` model (recursive calls share closure_env, param bindings leak between calls). Flat environments in Phase 2 make reuse trivially safe. (`src/eval.rs:527-529`) [Major, performance-expert, deferred]
- [ ] Bounded Display depth for Value — limit `Value::Dict` Display impl to max 3 levels, print `...` beyond that to prevent deep traversal in error messages (`src/value.rs:113-143`) [Minor, performance-expert]
- [ ] Document performance characteristics in DESIGN.md — Environment O(depth) lookup, IndexMap ~20% vs HashMap, thunk triple-boxing cost, Substitution::apply() tree walk cost [Minor, performance-expert]
- [x] Decide Substitution::apply() depth limit behavior — Option A: raise TypeError on >256-depth chains ("type substitution exceeded maximum depth"). Analogous to MAX_EVAL_DEPTH → EvalError. Silent truncation defeats purpose of type checking. OCaml/Haskell precedent. Union-find migration will subsume.
- [ ] Add per-variable depth limit to Substitution::apply() — implement chosen behavior (`src/types.rs:141-144`) [Critical, type-theorist]
- [ ] Fix Substitution::apply depth counter conflating chain depth with structural width — single `depth` parameter increments for both TypeVar chain-following and structural descent into Record fields/Function params. A record type with >256 fields (K8s manifests) would silently return un-substituted type variables. The `visited: HashSet<String>` already prevents infinite TypeVar chains (Tarjan 1975), so depth counter should only guard structural recursion. Either increment depth only on TypeVar resolution, or use separate counters. (`src/types.rs:145-198`) [Major, computer-scientist]
- [ ] Document Environment DAG invariant — add doc comment and debug-mode cycle detector (`src/value.rs:333-392`) [Major, eval-engine]
- [ ] Cache four-pass dict inference key resolution — `infer_expr` resolves keys twice across passes (`src/typecheck.rs:272-295`) [Minor, type-theorist]
- [ ] Add clarifying comment to `bind_args_thunks` double conflict check (`src/eval.rs:573-587`) [Nit, eval-engine]
- [ ] Consider `matches!` instead of `!=` in `key_in_range` (`src/eval.rs:22-50`) [Nit, eval-engine]
- [ ] Extract `MAX_APPLY_DEPTH` constant to shared location — duplicated in `src/types.rs:127` and `src/eval.rs:42` [Nit, performance-expert]
- [ ] Avoid AST clone in eval_call argument thunk creation — change `CallExpr` args to `Rc<Spanned<Expr>>` so eval_call does `Rc::clone` instead of deep-cloning AST subtrees per argument. Internal refactor to ast.rs/parser.rs, backward-compatible at public API. ~20-40% call overhead reduction. (`src/eval.rs:416-435`) [Major, performance-expert, design-review promoted]
- [ ] Avoid AST clone in `Expr::Fn` body — `body.as_ref().clone()` deep-clones entire body AST subtree on every function creation. For closures in loops (lambda args to `$map`/`$filter`), repeated allocation cost. Change `Expr::Fn.body` from `Box<Spanned<Expr>>` to `Rc<Spanned<Expr>>` in AST definition, then `Rc::clone(body)` instead of deep-cloning. Consolidate with call-arg Rc migration above. (`src/eval.rs:170-171`, `src/ast.rs:106`) [Major, computer-scientist]
- [ ] Reduce materialize() return-path cloning — `value.clone()` on every return stores into `Materialized(value.clone())` then returns the original; could instead store `Materialized(value)` then clone from cached state via `try_get_materialized()`. For Dict, eliminates O(n) Rc::clone of entries on the return path. (`src/eval.rs:918,939,944,990,1014,1020`) [Major, performance-expert]
- [ ] Optimize `cache_failure` to skip clone when already Failed — `cache_failure()` always creates fresh Failed state with `Box::new(err.clone())`, even if thunk is already Failed with identical error. Common in deep call chains where same error propagates through multiple layers. Check current state and skip clone if already `Failed`. (`src/value.rs:384-386`) [Major, eval-engine]
- [ ] Avoid intermediate Vec in value_to_display_string — collects all formatted entries into Vec<String> then joins; write directly to String with_capacity instead (`src/lib.rs:194-204`) [Minor, performance-expert]
- [ ] Avoid intermediate Vec in builtin_split — `input.split(sep).collect::<Vec<&str>>()` then maps to thunks; use iterator directly (`src/builtins.rs:525-535`) [Nit, performance-expert]
- [ ] Cache materialized dict in `builtin_cycle_step` — currently re-materializes immutable dict on every iteration; store IndexMap directly (`src/builtins.rs:1375-1443`) [Major, performance-expert]
- [ ] Optimize `builtin_take` Dict path — materializes entire dict then clones first n entries; use `map.iter().take(n)` directly (`src/builtins.rs:1648-1657`) [Major, performance-expert]
- [ ] Consider SmallVec for sequence constructor tail args — `Vec::new()` + push allocates heap on every step of infinite sequences; SmallVec<[Rc<Thunk>; 2]> would stack-allocate common cases (`src/builtins.rs`) [Minor, performance-expert]
- [ ] SmallVec for eval_call positional args — most calls have ≤4 args; SmallVec<[Rc<Thunk>; 4]> avoids heap allocation for common case (`src/eval.rs:417-426`) [Minor, performance-expert]
- [ ] SmallVec for error stack frames — SmallVec<[StackFrame; 8]> for shallow stacks (`src/error.rs`) [Minor, performance-expert]
- [ ] Thunk origin String→Option<Rc<str>> — most thunks have empty origin; eliminates per-thunk allocation, share origin strings via Rc (`src/value.rs:204,216`) [Nit, performance-expert]
- [ ] Add capacity hint to variadic dict allocation — exact size known at allocation time (`src/eval.rs:610`) [Minor, performance-expert]
- [ ] Use static empty IndexMap for PendingCall named args — eliminates allocation on every PendingCall with no named args (`src/eval.rs:976`) [Nit, performance-expert]
- [ ] Use static empty dict thunk for default `$$` — eliminates allocation on every file eval without stdin (`src/eval.rs:287-291`) [Nit, performance-expert]
- [ ] Builtin strictness annotations — classify each builtin's argument strictness (strict/lazy per position); arithmetic, comparison, and string builtins are strict in all args, `$if` strict in condition only, `$seq` lazy in both. Strict args can skip thunk allocation in eval_call. Level 1 optimization per Mycroft (1981). (`src/eval.rs:414-438`, `src/builtins.rs`) [Major, computer-scientist]
- [x] Research Rc cycle leak mitigation strategy — resolved by arena allocation design in DESIGN.md §Allocation Strategy. Section-scoped arenas eliminate Rc cycles within evaluation; selective migration at `---` boundaries handles cross-section values. No separate proposal needed.

## iterative-eval: Iterative Evaluator

Replace the recursive `eval()` / `materialize()` call stack with an explicit continuation stack (stack machine). Nix, Nickel, and Jsonnet all use iterative evaluation with explicit frame types. LLT's recursive approach risks stack overflow on deeply-nested lazy chains and prevents tail-call optimization.

- [x] Design `Frame` enum for explicit continuation stack — see DESIGN.md §Iterative Evaluator — Defunctionalized CPS (CEK Machine). Uses `Action` enum (Eval/Materialize/Continue) + `Cont` enum (~18-20 defunctionalized continuation variants, boxed large fields for ≤96B frames) in an iterative two-register loop. Agent-reviewed: eval-engine, laziness-auditor, performance-expert.
- [x] Research safe Rust arena patterns for thunks/environments — see doc/whatif/arena-patterns.md. Recommends hand-rolled `Vec<Thunk>` + `ThunkId(u32)` with RefCell (cranelift entity pattern). typed-arena/bumpalo can't handle cyclic graphs; GhostCell ergonomic cost prohibitive; slotmap/thunderdome add unnecessary deletion overhead. 4-step adoption: variable resolution → arena types → CEK machine → selective migration
- [x] Design arena lifetime policy for REPL/LSP — arena lifetime = one document section (between `---` boundaries). At `---`, selectively migrate `$$`-reachable thunks from arena to Rc-backed storage (preserves laziness, closures, infinite sequences), bind as `$$`, drop arena. See DESIGN.md §Allocation Strategy.
- [ ] Environment reuse in bind_args_thunks — safe with flat environments (each call writes to own activation frame). Deferred from perf-foundations where it was unsafe with shared `Rc<RefCell<Environment>>`. (`src/eval.rs:527-529`)
- [ ] Fix DESIGN.md `Cont::PendingCallForceFunc` to include `named: Box<IndexMap<String, Rc<Thunk>>>` — PendingCall now carries named args (commit b6c06b5) but the CEK Cont sketch omits them; defunctionalized continuation must capture all free variables of the original closure (Reynolds 1972) (`DESIGN.md §Iterative Evaluator`) [Minor, computer-scientist]
- [ ] Fix arena-patterns.md `FlatEnv` O(1) lookup claim — claims `env.slots[slot]` is O(1) but `FlatEnv` has a `parent: Option<EnvId>` chain and no display vector. Either add display vector (classic de Bruijn 1972) or specify copy-on-capture flat closures (Nix model, O(scope_size) creation cost). (`doc/whatif/arena-patterns.md:258-266`) [Minor, computer-scientist]
- [ ] Convert `materialize()` from recursive to iterative with `Vec<Frame>` work stack
- [ ] Convert `eval()` hot paths (dict construction, access chains) to iterative
- [ ] Implement tail-call optimization (TCO) for `call` expressions — detect tail position, reuse frame
- [ ] TCO for recursive stdlib functions (`fold`, `map`, `filter`, `sort-merge`) to avoid stack overflow on large inputs
- [ ] Benchmark: compare recursive vs iterative on deep chains and large collections
- [ ] Remove 64MB worker thread stack workaround once iterative eval eliminates deep recursion
- [ ] Verify thunk lifecycle invariants after CEK migration — sharing preservation (thunk identity via `Rc<Thunk>` must be maintained through continuation dispatch), ThunkState simplification (PendingBuiltin/PendingCall subsumed by Cont variants), MAX_EVAL_DEPTH removal (replace with configurable `--max-depth`), monotonicity proof carries over. See DESIGN.md §Thunk Lifecycle — Relationship to CEK Machine Migration. [Major, computer-scientist]

## row-unification: Full Row-Variable Unification (Remy-Style)

Replace the current closed-strict/open-lenient record unification with full Remy-style row-variable unification. Row variables become first-class participants in type inference, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

**Benefits from:** `type-extensions` Type::Seq inference (for sequence type support in row polymorphism).

- [x] Design Remy-style row unification model (row variable binding, remainder semantics, occurs check) — see DESIGN.md §Row-Variable Unification — Kinded Rémy Model (Dict+Tail Representation)
- [ ] Extend `Substitution::apply` to splice bound row variable fields into records (e.g., `[a: Int | ...r]` with `r → [b: String]` produces `[a: Int, b: String]`)
- [ ] Unify row rests: `RowVar` vs `RowVar` binds one to the other, `RowVar` vs `Closed` binds the var to the leftover fields as a closed record
- [ ] Handle "remainder" binding: `unify([a: Int | ...r], [a: Int, b: String | Closed])` binds `r → [b: String | Closed]`
- [ ] Extend `Type` representation if needed to support partial-row bindings (row var bound to fields + another row var)
- [ ] Update `instantiate` to freshen row variables alongside type variables
- [ ] Test inference through polymorphic functions that extend/restrict records (e.g., `[fn add-id [r@[...rest]] [id: 1  ...rest]]`)
- [ ] Verify consistency between `unify` and `is_subtype` for all RowRest combinations
- [ ] Add row-specific occurs check for `RowVar("r")` with `Record(..., RowVar("r"))` (infinite row type prevention)
- [ ] Add row variable substitution cycle handling — `Substitution::apply` must handle cycles when row variables bind to records containing the same row variable (`src/types.rs`) [Major, type-theorist]
- [ ] Cache FTV/FRV (free type/row variables) per Type during construction — row-variable occurs check is O(n×m) for n fields × m type depth without caching; 500-field records (K8s manifests) create a hot path. With cached sets, occurs check becomes O(1) set membership. [Major, performance-expert]
- [ ] Use HashMap for Row.fields at type level, IndexMap only at runtime — row fields are semantically unordered (Rémy left-commutativity), IndexMap's insertion-order preservation adds ~20% overhead unnecessary in the type checker. Runtime `Value::Dict` keeps IndexMap for user-visible key ordering. [Minor, performance-expert]
- [ ] Subtyping proof search for TypeAssert defaults — validate default value type matches asserted type (type-theorist review)
- [ ] Fix row variable substitution creating duplicate fields — `merged.extend(extra_fields)` doesn't check for key collisions (`src/types.rs:166-184`) [Critical, type-theorist]
- [ ] Fix RowVar treated identically to Open in `is_subtype` — add TODO comment explaining row-unification placeholder (`src/types.rs:59-62`) [Major, type-theorist]
- [ ] Fix sub_rest ignored in record subtyping — `is_subtype` destructures sub record as `(sub_fields, _sub_rest)`, ignoring RowVar/Open rest; a record with `RowVar(r)` may have additional fields via its row variable not checked against a Closed supertype. Latent issue that becomes real with row variable binding. (`src/types.rs:52-64`) [Major, computer-scientist]
- [ ] Document RowVar instantiation via TypeVar namespace coincidence — `instantiate()` renames TypeVar names and RowVar names share the same namespace, so RowVars get freshened correctly by accident. Should be documented as intentional or given separate namespace handling. (`src/types.rs:318-330`) [Minor, computer-scientist]
- [ ] Generate unification constraints from access chains — `$x.field` should produce `unify(typeof(x), Record([field: α], ρ))`, enabling field requirement inference from usage. Currently access type checking is direct lookup (limitation, not design choice). See DESIGN.md §Access Chain Evaluation Part 5. (`src/typecheck.rs:297-357`) [Major, type-theorist]
- [ ] Fix open-record unification silently dropping non-shared fields — only shared fields unified, unique fields ignored without constraint; Remy-style would bind fresh row variables to capture remainders (`src/types.rs:334-338`) [Major, computer-scientist]
- [ ] Ensure row variable occurs check before binding — existing `occurs_in()` handles `RowVar` case correctly (`src/types.rs:215-228`), but when row-unification adds row variable binding, it must go through the same occurs-check path to prevent infinite types like `r = [x: Int ...r]`. (`src/types.rs`) [Major, computer-scientist]

## sandbox: Sandboxing & Security

Design and implement four unprivileged sandboxing layers. See DESIGN.md §Sandboxing & Security for full design.

**Depends on:** `evalcontext-refactor` for filesystem allowlist in EvalConfig.

- [x] Design sandboxing model — see DESIGN.md §Sandboxing & Security
- [x] Decide policy for absolute paths — allowed if within any --allow-path
- [x] Decide policy for symlinks — canonicalize, then check against allowlist
- [ ] Implement filesystem allowlist in `EvalConfig` (depends on evalcontext-refactor)
- [ ] Add path-ancestor allowlist check in `builtin_include` (after canonicalize, before cache)
- [ ] Add `--allow-path` global CLI flag (default: `.`)
- [ ] Implement Landlock filesystem ACLs (Linux 5.13+, graceful degradation)
- [ ] Implement seccomp-bpf network sandbox (block socket/connect/bind/listen/accept)
- [ ] Implement seccomp-bpf process sandbox (block fork/execve/execveat, allow clone)
- [ ] Implement rlimit resource caps (RLIMIT_AS, RLIMIT_CPU eval-only, RLIMIT_NOFILE, RLIMIT_FSIZE)
- [ ] Add `--allow-network`, `--max-memory`, `--max-cpu`, `--max-fds` global CLI flags
- [ ] Test: relative paths within allowlist succeed
- [ ] Test: `../` traversal beyond allowlist fails
- [ ] Test: absolute paths outside allowlist fail
- [ ] Test: symlinks pointing outside allowlist fail
- [ ] Test: graceful degradation when Landlock/seccomp unavailable

## type-extensions: Type System Extensions

Future type system work identified by type-theorist review (2026-04-19). Updated by type-theorist review (2026-04-22).

- [x] Design type system extension roadmap (Seq types, gradual typing, type classes, error recovery) — see DESIGN.md §Type System Extension Roadmap, `doc/whatif/gradual-typing.md`, `doc/whatif/typeclasses.md`, `doc/whatif/union-types.md`
- [ ] Add Type::Seq inference to typecheck.rs — sequence builtins ($seq, $range, $repeat, $cycle, $iterate, $unfold, $take) currently infer as Any; annotate return types in `check_call` for LSP hover and type safety (`src/typecheck.rs`) [Major, type-theorist]
- [ ] Fix polymorphic call unification for named args — requires extending `Type::Function` to carry param names; after positional unification, named args are not unified (`src/typecheck.rs:389-447`) [Major, type-theorist, deferred from types-major-fixes]
- [ ] Gradual typing with Any→concrete boundary tracking and blame (TypeScript/Typed Racket model)
- [x] Research polymorphic recursion detection — moot without algebraic data types. Polymorphic recursion requires parametric recursive type constructors (e.g., `Nested a → Nested [a]`); tinct has none and none are planned. The monomorphic letrec restriction (Limitation #6) is correct. Revisit only if sum types or user-defined recursive type constructors are added.
- [ ] Type error recovery with `Type::Error` sentinel that doesn't unify (prevents cascading errors, improves LSP)
- [ ] Type class constraints for arithmetic/comparison (needed if user-defined types get custom operators)
- [x] Decide builtin type signature representation — use `Any` for all polymorphic parameter positions, precise return types where known (e.g., `$= : Any → Any → Bool`, `$/ : Any → Any → Float`, `$not : Bool → Bool`). Defer precise input types until algebraic subtyping or type classes exist. Forward-compatible — refine signatures when union types land.
- [ ] Add `TypeEnv::with_builtins()` constructor pre-registering builtin type signatures — builtins are not registered in `TypeEnv::new()`, so the type checker cannot validate user code using `$=`, `$<`, etc. (`src/types.rs:420-426`, `src/typecheck.rs:149-152`) [Major, type-theorist]
- [x] Research typeclass-based equality/ordering constraints — see doc/whatif/typeclasses.md. Key decisions: keep `$=` as EQ-INCOMP for dicts (no breaking change), add `$deep-eq` (short-circuiting structural) and `$shallow-eq` (pointer identity for thunks). Key-set equality (order-independent). Constrained row variables deferred to typeclass adoption.
- [ ] Define hash consistency requirements for Dict key equality — `hash(a) == hash(b)` whenever `Value::PartialEq` says `a == b` (NOT `$=` user-facing equality). Int and Float use separate hash paths even when numerically equal via promotion, so `[1: x]` and `[1.0: y]` are distinct keys. Document before implementing Dict key deduplication or Set types. [Minor, type-theorist]
- [x] Decide type alias shadowing policy — allow lexical scope shadowing (inner alias shadows outer). Consistent with value binding semantics. Same-dict redefinition already caught by duplicate key check. OCaml/Haskell/TypeScript precedent.
- [ ] Type environment alias registry shadowing policy — implement chosen policy (`src/types.rs:433-435`) [Major, type-theorist]
- [ ] `type-of` returns "Dict" for all dicts, no list discrimination — document in Future Features (`src/builtins.rs`, `DESIGN.md:1710`) [Minor, stdlib-author]
- [ ] Fix type display for empty open record — `[...]` is ambiguous, consider `[... (open)]` notation (`src/types.rs:359`) [Minor, type-theorist]
- [ ] Make `TypeEnv::lookup` `pub(crate)` — currently private but useful for testing (`src/types.rs:415-427`) [Minor, type-theorist]
- [ ] Document `Substitution::get` being `cfg(test)` only — either make always-public or explain opaqueness (`src/types.rs:198-202`) [Minor, type-theorist]
- [ ] Fix instantiation counter overflow — `u32` theoretically overflows; use `u64` or document assumption (`src/types.rs:318-330`) [Minor, type-theorist]
- [ ] Document `Type::Number` having no literal variant — asymmetry with Int/String is due to dict key constraint (`src/types.rs:21-37`) [Minor, type-theorist]
- [ ] Fix `Type::Function` Display for nested types — add parentheses for nested function annotations (`src/types.rs:369-378`) [Minor, type-theorist]
- [x] Decide variadic param annotation semantics — forbid annotations on `...args`. Row types use string keys but variadic collects into Int-keyed Dict; annotation can't participate in type inference. Revisit when Seq types land (variadic may collect into `Seq<T>` instead of Dict).
- [ ] Fix variadic param type from `Record([], Closed)` to `Any` — no annotation to resolve, just correct the type (`src/typecheck.rs:469-473`) [Minor, type-theorist]
- [ ] Clarify `resolve_annotated` interpreting all Fn annotations as function types (`src/typecheck.rs:522-533`) [Minor, type-theorist]
- [ ] Populate type map on errors — record `Type::Any` for failed subexpressions to improve LSP hover (`src/typecheck.rs:200-206`) [Minor, type-theorist]
- [ ] Consider `HashSet` instead of `BTreeSet` in `collect_type_vars` — order doesn't matter (`src/types.rs:85-106`) [Nit, type-theorist]
- [ ] Remove unused `Substitution` from `instantiate` return type — or document why returned (`src/types.rs:318-330`) [Nit, type-theorist]
- [ ] Document `Type::is_subtype` not short-circuiting on `Any` in nested positions (`src/types.rs:42-83`) [Nit, type-theorist]
- [ ] Fix type display using two spaces between fields — consider single space (`src/types.rs:345-367`) [Nit, type-theorist]
- [ ] Fix DESIGN.md "pure Robinson" unification claim — DESIGN.md §Unification claims unification is pure Robinson with subtyping handled by [U-SUBSUME]/`check_expr`, but code implements 8 bidirectional literal promotion rules directly in `unify()` (`IntLiteral↔Int`, `IntLiteral↔Number`, `Int↔Number`, `Float↔Number`, `IntLiteral↔Float`, `StringLiteral↔Str`). When `bidirectional-typing` lands, either remove promotions from `unify()` and rely on [SUB]/[U-SUBSUME], or update DESIGN.md to document pragmatic approach as intentional. (`src/types.rs:263-289`, `DESIGN.md`) [Major, type-theorist]
- [ ] Add comment explaining `IntLiteral(n) ~ Float` literal-specific promotion (`src/types.rs:263`) [Nit, type-theorist]
- [ ] Fix IntLiteral-Float edge case: `unify` accepts `(IntLiteral, Float)` but `is_subtype` rejects it — when bidirectional-typing lands and promotions are replaced by `is_subtype` fallback, `unify(IntLiteral(42), Float)` will start failing. Either add `IntLiteral <: Float` to `is_subtype` or remove the arm from `unify` now. (`src/types.rs:289` vs `src/types.rs:42-84`) [Nit, computer-scientist]
- [ ] Fix `TypeEnv::with_parent` taking `Rc` instead of `&Rc` — minor API ergonomics (`src/types.rs:399-405`) [Nit, type-theorist]
- [ ] Add `Eq` derive to `TypeError` (`src/types.rs:444-448`) [Nit, type-theorist]
- [ ] Document `TypeMap` using `(offset, offset)` as key instead of `Span` — offsets are sufficient (`src/typecheck.rs:16`) [Nit, type-theorist]
- [ ] Consider `Result<Type, TypeError>` for `infer_expr` match arms — most wrap single error in vec (`src/typecheck.rs:142-209`) [Nit, type-theorist]
- [ ] Document `check_call` not verifying named args exist in params — intentional: named args are eval-time (`src/typecheck.rs:389-447`) [Nit, type-theorist]
- [ ] Consider `HashMap` instead of `IndexMap` for type alias registry — order doesn't matter (`src/types.rs:386`) [Nit, type-theorist]
- [ ] Clarify `Fn@T` with zero params — document whether it means thunk or nullary function (`src/typecheck.rs:536-541`) [Nit, type-theorist]
- [x] Research Type::Any consistency vs subtyping separation — see doc/whatif/gradual-typing.md. Covers consistency relation (Siek & Taha 2006), AGT framework (Garcia et al. 2016), is_consistent() vs is_subtype() separation, Any→Unknown+Top split. Recommendation: don't adopt now; revisit when Any causes a real false positive or algebraic subtyping is adopted.
- [ ] Document principal type property violations — LLT does not satisfy Damas-Milner principal type theorem: (1) no let-generalization, (2) non-MGU literal coercions, (3) subtyping + parametric polymorphism interaction, (4) Type::Any as universal unifier. Document as known limitation in DESIGN.md §Type Inference. [Minor, computer-scientist]
- [ ] Document literal promotion symmetry in unification — `IntLiteral↔Int` unification is bidirectional; in a subtyping-aware system `IntLiteral <: Int` but not vice versa; reduces diagnostic value (`src/types.rs:263-264`) [Minor, computer-scientist]
- [x] Research path-sensitive type narrowing — see doc/whatif/narrowing.md. Make `$if` a type-level special form, fork type environments per branch. Four narrowing patterns: equality-with-literal, type-of guard, key presence, boolean conjunction. No false-branch narrowing (needs negation types). Assumes typeassert-structural complete. Trigger: after let-generalization + bidirectional-typing.

## theoretical-foundations: Theoretical Foundations (Computer-Scientist Review)

Findings from formal audit of DESIGN.md theoretical claims (2026-04-21). Covers type theory, evaluation semantics, and research grounding.

### Design work (requires design loop)

- [x] Design type inference algorithm specification — see DESIGN.md §Type Inference Algorithm. Semi-formal spec with judgment rules: type grammar, 14 inference rules (INT, FLOAT, BOOL, STR, VAR, DICT, FN, CALL-MONO, CALL-POLY, CALL-ANY, DOT, BRACKET, RANGE, ASSERT, ALIAS, ANNOTATED), unification rules (Robinson-style + silent coercions), subtyping rules (S-ANY-TOP/BOT, S-REC, S-FN with variance), instantiation with row variable renaming, 8 documented limitations. Agent-reviewed: computer-scientist + type-theorist. [Critical, computer-scientist]
- [x] Design let-generalization for proper HM inference — see DESIGN.md §Let-Generalization (Levels-Based). Kiselyov (2013) levels-based approach with InferState, symmetric level lowering, Any-unification level zeroing, TypeScheme in TypeEnv, RowVar levels, document-level scheme threading. Agent-reviewed: computer-scientist + type-theorist + integration-verifier (18 findings, all applied). [Major, computer-scientist]
- [x] Design literal promotion semantics in unify() — see DESIGN.md §Bidirectional Typing + §Unification [U-SUBSUME]. Bidirectional type checking (Pierce & Turner 2000; Dunfield & Krishnaswami 2021): synthesis/checking modes, check_expr for concrete positions (CALL-MONO, TypeAssert, return annotations), unify with [U-SUBSUME] for CALL-POLY. Pure Robinson unification + bidirectional subsumptive fallback for concrete pairs. Singleton literal types (not refinement types). Agent-reviewed: computer-scientist + type-theorist (confluence fix applied). [Critical, computer-scientist]
- [x] Design TypeAssert static/runtime consistency — see DESIGN.md §TypeAssert Runtime Validation. Full structural convergence: elaboration (Dunfield & Krishnaswami 2021) embeds resolved type in AST node, proxy contracts (Findler & Felleisen 2002) wrap record field thunks in guards for lazy type validation on access. New ThunkState::Guarded variant. Consistency invariant qualified for deeply checkable types. Agent-reviewed: computer-scientist + type-theorist + eval-engine (2 rounds, all findings applied). [Major, computer-scientist]
- [x] Design `$_` desugaring as formal transformation — see DESIGN.md §`$_` Desugaring — Formal Specification. Pre-typecheck AST pass (parse → desugar → typecheck → eval). Top-down WRAP check on raw children with DIRECT predicate, depth-based lexical shadowing replaces eval-time env check. Corrected traversal avoids greedy-wrapping (Visser 1998). Type visibility qualified for current (Any → T) and future (bidirectional, row-polymorphic) inference. Agent-reviewed: computer-scientist + type-theorist + eval-engine + grammar-architect + integration-verifier (2 rounds, all findings applied). [Minor, computer-scientist]
- [x] Design sequence productivity obligations — see DESIGN.md §Productivity Obligations. Pragmatic approach: no static guarantee (Haskell/Nix model), three-layer runtime protection (blackholing, depth limit, tail discipline), productive-by-construction combinators as primary API, documented user obligations for `$seq`. Static checking rejected: totality (Turner 2004) sacrifices Turing-completeness, sized types (Abel & Pientka 2013) incompatible with HM. Agent-reviewed: computer-scientist + type-theorist + eval-engine (1 round, all findings applied). [Minor, computer-scientist]

### Formal specifications (additional)

- [x] Design formal specification of thunk lifecycle — see DESIGN.md §Thunk Lifecycle — Formal Specification. State transition DAG with monotonicity proof, 9 forcing rules (including FORCE-CALL-BUILTIN), 6 semantic properties, 4 explicit semantic commitments, adequacy argument for PendingBuiltin/PendingCall via defunctionalization (Reynolds 1972). Agent-reviewed: computer-scientist + type-theorist + eval-engine (all findings applied). [Major, computer-scientist]
- [x] Design formal specification of selective materialization — see DESIGN.md §Selective Materialization — Formal Specification. Two-tier spec: strictness signature table (Mycroft 1981) for all 44 builtins with S/L/Sc per-argument annotations + delta rules (Plotkin 1981 SOS) for 10 non-trivial builtins (14 rules). Five result classifications (→ V/D/Θ/LT/⊥), dual-dispatch pattern for 6 collection builtins, derived selectivity for 7 stdlib functions with inheritance proof sketch via DELTA-IF inlining. Four properties: branch isolation, strictness monotonicity, sharing preservation, dual-dispatch consistency. Agent-reviewed: computer-scientist + type-theorist + eval-engine + laziness-auditor (14 findings applied). [Major, computer-scientist]
- [x] Design formal specification of call convention — see DESIGN.md §Call Convention — Formal Specification. Dual-layer spec: declarative binding constraints + phased algorithm + complete correctness proof (uniqueness, soundness, completeness by case analysis). Kotlin model: any param nameable, interleaved required/optional allowed, per-parameter coverage check (C-COVERAGE) replaces count-based arity. Garrigue (1995) default-env separation. 6 constraints, 5 phased rules, 4 error classes, worked example. Agent-reviewed: computer-scientist + type-theorist + eval-engine (17 findings applied, including critical C-ARITY→C-COVERAGE rewrite). [Major, computer-scientist]
- [x] Design formal specification of scope chain semantics — see DESIGN.md §Scope Chain Semantics — Formal Specification. Launchbury (1993) natural semantics + Nakata & Hasegawa (2009) cyclic call-by-need. Three construction rules (DICT-SCOPE letrec, SEQ-SCOPE let*, DOC-PIPELINE $$) + LOOKUP with parent-chain walk. Five properties with proof sketches: shadowing correctness, mutual visibility (letrec sharing + construction-time non-forcing invariant), heap monotonicity, scope chain acyclicity (parent chain vs Rc capture graph distinction), determinism (Ariola-Felleisen confluence via Launchbury adequacy). Referential integrity corollary: scope-chain and dict-field access share same Rc<Thunk>. Type system parallel cross-reference to TypeEnv/let-generalization. Agent-reviewed: computer-scientist + type-theorist + eval-engine + laziness-auditor (all findings applied, including construction-time non-forcing invariant, DOC-PIPELINE depth parameterization, FORCE-DEPTH context-sensitivity). [Major, computer-scientist]
- [x] Design formal specification of access chain evaluation — see DESIGN.md §Access Chain Evaluation — Formal Specification. Access algebra with compositional chain semantics: projections (dot, bracket, range) composed left-to-right, parser produces nested AST nodes reduced inside-out. FORCE-DICT shared forcing step + three projection rules. Five chain properties: step-wise forcing, result laziness, error short-circuiting, depth consumption, sharing preservation (Launchbury). Type system correspondence: direct lookup (not constraint generation), type variable access is error (pre-row-unification), open record → Any via gradual typing, range type preservation sound by structural subtyping. Agent-reviewed: computer-scientist + type-theorist + eval-engine (all findings applied). [Minor, computer-scientist]
- [x] Design formal specification of equality and comparison — see DESIGN.md §Equality and Comparison — Formal Specification. Two primitive relations: EQ (total, cross-type Int/Float promotion via `as f64`) and LT (partial, errors on incompatible types). Type-dispatch tables with 7 rules each, derived relations ($>, $<=, $>=) via negation in stdlib. IEEE 754 NaN analysis with documented $<= / $>= anomaly (negation-based derivation), NaN entry path via $from-json → Infinity → arithmetic. Key::PartialOrd as pre-materialization optimization. Value::PartialEq vs $= divergence with implementation guidance. 10 algebraic properties including P3 transitivity WARNING at 2⁵³ boundary, P7 cross-type trichotomy failure. Agent-reviewed: computer-scientist + type-theorist + eval-engine (12 fixes applied, 6 deferred to type-extensions/float-nan-infinity/row-unification). [Minor, computer-scientist]
- [x] Design formal specification of $merge — see DESIGN.md §Merge — Formal Specification. Right-biased merge (L ⊕ R) with insertion-order preservation. Typing rules: T-MERGE for closed records (Record(F_L ⊕ F_R, Closed)), T-MERGE-ANY gradual fallback, forward-compatibility for row variables with 3 constraints (closed-record preservation, common-tail preservation, principality). 8 algebraic properties including associativity on both content and iteration order (with proof), monoid over ordered maps (Dict, ⊕, ∅), value preservation (Rc::clone). Lazy overlay compatibility: 3 behavioral equivalence constraints, 2 documented observable differences (error timing, error ordering), overlay chain depth exempt from MAX_EVAL_DEPTH. Harper & Pierce (1991) disjointness relaxed to right-bias, Rémy (1994) presence/absence alternative noted. Agent-reviewed: computer-scientist + type-theorist + laziness-auditor (13 fixes applied: P4a wrong iteration-order caveat removed with proof, Key type corrected, list-dict behavior noted, T-MERGE closed-record restriction explicit, TypeVar fallback specified, row-variable constraints strengthened, T-MERGE-ANY rationale documented, overlay error timing/ordering corrected, overlay strictness dual-noted, chain depth exemption specified, sharing constraint strengthened to Rc::ptr_eq, error message format matched). [Minor, computer-scientist]
- [x] Design formal specification of error semantics — see DESIGN.md §Error Semantics — Formal Specification + SPEC.md §9 Runtime Errors. Dual-span error model (def_span + mat_span + stack). 6 error constructors. DECORATE rules with deduplication guards (expanded notation ∄f ∈ ε.stack) + idempotence property (E8) + origin cross-reference to §Scope Chain Semantics. 6 propagation rules: PROP-EVAL (with recursive materialize note), PROP-BUILTIN, PROP-RESULT (with PendingCall coverage note for 4 error paths), PROP-CYCLE (circular dependency, bypasses DECORATE — inline construction), PROP-DEPTH (non-caching). MEMO-CACHE + MEMO-REACCESS with mat_span=None case. $try catching boundary with typing (Any → Any, Phase 3+ for union result type). $error typing (Str → Any, no bottom type). 8 properties (E1-E8). Runtime vs static error distinction (EvalError vs Type::Error). SPEC.md §9: error structure with dual-span example, display format with frame ordering clarification, 9 exhaustive error categories with stability disclaimer, representative builtin-specific errors table with operator prefix note, $try catching, lazy error behavior, 6 span assignment corrections. Agent-reviewed: computer-scientist + type-theorist + eval-engine + span-integrity-checker (17 fixes applied: PROP-CYCLE added, DECORATE bypass noted, PendingCall 4-path coverage, PROP-EVAL conflation note, MEMO-REACCESS None case, E2 Option notation, DECORATE notation expanded, E8 idempotence, origin cross-ref, $try/$error typing, Type::Error clarification, SPEC operator prefixes, representative table note, 2 additional §9.6 findings, §9.6 row 3 fix, §9.1 example, §9.2 frame order). [Major, computer-scientist]
- [x] Design formal specification of document pipeline and $include — see DESIGN.md §Document Pipeline and $include — Formal Specification. Cross-references DOC-PIPELINE and SEQ-SCOPE (§Scope Chain Semantics). Include state Σ = ⟨base_dir, guard, cache, stdlib_env⟩ with thread-local model and EvalContext migration note. RESOLVE rule with canonicalization. Three include rules: INCLUDE-HIT (cache, Jsonnet-style memoization), INCLUDE-CYCLE (guard set detection), INCLUDE-EVAL (fresh eval with file size check, guard push/pop, base_dir save/restore, eager materialization). Allowlist forward reference to §Sandboxing (planned INCLUDE-DENY). Eager materialization invariant: $include is one of three builtins ($eval, $try) that eagerly materialize — required because guard/base_dir are stack-scoped but thunks outlive stack frames; 3 failure modes documented (cycle detection, path resolution, cache coherence). materialize vs deep_materialize distinction. 5 properties: P1 cycle detection termination (well-foundedness), P2 cache determinism with failure non-caching note (two independent caching levels), P3 guard restoration with known defect (materialize error path), P4 include determinism (conditional on filesystem, SC-2), P5 include isolation (stdlib_env only, empty $$). Agent-reviewed: computer-scientist + eval-engine + laziness-auditor (11 fixes applied: SC-5→SC-2, allowlist forward ref, file size check, parameter s defined, failure non-caching, no-eval window note, materialize error path defect documented, line range fix, builtin count corrected, materialize/deep_materialize distinction, $$ indirection clarified). [Major, computer-scientist]

### Proof obligations

- [ ] Mechanized proof of thunk lifecycle adequacy — formalize bisimulation between PendingBuiltin/PendingCall and equivalent Unevaluated thunks, confirming defunctionalization preserves semantics (Reynolds 1972, Danvy & Nielsen 2003). Property-based testing (QuickCheck-style) as a first step; full Coq/Isabelle/HOL formalization as stretch goal. See DESIGN.md §Thunk Lifecycle — Adequacy. [Minor, computer-scientist]
- [ ] Confluence proof sketch for pure subset — show that forcing order does not affect final values in tinct programs without `$include`. The PendingBuiltin/PendingCall extensions add new reduction paths that must preserve the Ariola & Felleisen (1997) diamond property. See DESIGN.md §Thunk Lifecycle — Semantic Properties. [Minor, computer-scientist]

### Code fixes

- [ ] Fix variadic parameter typing as closed empty record — `...args` typed as `Record(IndexMap::new(), RowRest::Closed)` (empty closed record); should be `RowRest::Open` or `Type::Any` to indicate arbitrary fields accepted. One-line fix. (`src/typecheck.rs:469-473`) [Major, computer-scientist]

## Stdlib Expansion

Missing functions identified by cross-language analysis (Jsonnet, jq, Nix, Dhall). All implementable in LLT unless noted.

### stdlib-missing-core: Core Missing Functions

- [ ] `with-entries` — `entries | map(f) | from-entries` pipeline (jq pattern; depends on `from-entries` from stdlib-pre-seq)
- [ ] `partition` — single-pass split into matching/non-matching dicts (Nix + Dhall)
- [ ] `flat-map` / `concat-map` — `flatten (map f xs)`, monadic bind for collections (Jsonnet + jq)
- [ ] `find-first` / `find-first-or` — first element matching predicate, with default (Nix)
- [ ] `group-by` — group elements by key function, returning dict of lists (Nix)
- [ ] `deep-merge` — recursive merge for configuration overlays (Jsonnet, RFC 7396)
- [ ] `walk` — recursive bottom-up transform of all sub-values (jq)

### stdlib-convenience: Convenience Functions

- [ ] `sum`, `min`, `max`, `count` — aggregate functions (one-liners over fold)
- [ ] `contains?` / `elem?` — membership test
- [ ] `uniq` / `unique` — deduplicate collection
- [ ] `foldr` — right fold (LLT only has left fold currently)
- [ ] `zip-with` — generalized zip with combining function; define `zip` as special case (Nix)
- [ ] `map-indexed` / `map-keys` — indexed mapping and key transformation (Jsonnet)
- [ ] `sort-on` — sort by key-extraction function instead of comparator (Jsonnet + Nix)
- [ ] `flip`, `abs`, `sign`, `clamp` — small composable primitives (Nix + Jsonnet; `const` moved to stdlib-pre-seq)
- [ ] `unzip` — inverse of zip, split list of pairs into pair of lists [Nit, stdlib-author C31]
- [ ] `transpose` — flip rows/columns of 2D structure [Nit, stdlib-author C31]
- [ ] `flatten-all` or depth parameter for `flatten` — current `flatten` only goes one level deep; add recursive variant or optional depth param [Nit, stdlib-author C31]
- [ ] `range-step` — range with step parameter; `$range` only supports `[start]` and `[start end]` with step=1 [Minor, stdlib-author C31]
- [ ] `take-while`, `drop-while` — take/drop elements while predicate holds; implementable via Seq constructor pattern like `filter` [Minor, stdlib-author C31]
- [ ] Variadic `all-of`/`any-of` — current `and`/`or` take exactly 2 args; add list-based variants `[fn [preds] [call $all? $identity $preds]]` [Nit, stdlib-author C31]

### stdlib-type-predicates: Type Predicates & Guards

- [ ] `is-int?`, `is-str?`, `is-float?`, `is-bool?`, `is-dict?`, `is-fn?` — type predicate wrappers over `$type-of` (Jsonnet pattern)
- [ ] Runtime assertion guards at stdlib function entry with descriptive errors (Jsonnet pattern)

### stdlib-numeric: Numeric Utilities

- [ ] `min`, `max`, `sum`, `product` — aggregate functions (stdlib-author review)
- [ ] `abs`, `sign`, `clamp` — numeric primitives (stdlib-author review)

### stdlib-string-ops: String Operations (requires new Rust builtin)

- [ ] Add `substr` / `slice-str` Rust builtin for substring extraction (unblocks below)
- [ ] `starts-with?`, `ends-with?` — string prefix/suffix tests
- [ ] `chars` — string to character sequence
- [ ] `join` — sequence/dict of strings to single string with separator

## Error Context Enrichment

Enhance error reporting with richer context types inspired by Elm, Nickel, and rustc patterns.

### error-restructuring: Error Model Restructuring

Core error model improvements. Foundation for all later error work.

- [x] Design structured error model (enum variants, error codes, style guidelines) — see DESIGN.md §Structured Error Model
- [x] Establish error message style guidelines (rustc's rules: no trailing punctuation, no questions, may contain names but not expressions) — see DESIGN.md §Structured Error Model Part 8
- [ ] Migrate freeform string error constructors to structured enum variants (`key_not_found`, `type_mismatch`, `arity_mismatch`)
- [ ] Add structured error codes (E001, E002, ...) for programmatic error filtering and documentation linking
- [x] Document dual-span error model in DESIGN.md — see DESIGN.md §Error Semantics — Formal Specification, Part 1: Error Representation
- [ ] Migrate lib.rs remaining `EvalError::new()` call sites to typed ErrorKind constructors — 5 sites at lines 110, 124, 161, 166, 191 still use escape hatch constructor instead of named ErrorKind constructors (`src/lib.rs`) [Minor, integration-verifier]
- [ ] Add builtin function name to error stack frames — builtin errors currently lack the function name in stack traces (`src/builtins.rs`, `src/error.rs`) [Major, span-integrity-checker]
- [ ] Deduplicate redundant span output when definition-site == materialization-site — show single span instead of identical pair (`src/error.rs`) [Major, span-integrity-checker]
- [ ] Add dual-span pattern to access chain errors — `DotAccess`, `BracketAccess` errors currently only report definition-site (`src/eval.rs`) [Major, span-integrity-checker]
- [ ] Fix builtin errors using call_span for definition-site — should use operand's span as definition-site, call_span as materialization-site (`src/builtins.rs:82-91`) [Major, span-integrity-checker]
- [ ] Fix builtin helper functions materializing with `None` mat_span instead of operand span — `expect_one_arg`, `extract_num_pair`, `require_dict`, `require_string` all call `materialize()` with `None`, losing dual-span error context. Should pass `Some(&args[i].span)`. (`src/builtins.rs:102,131-132`) [Major, span-integrity-checker]
- [ ] Fix `TypeMismatch::context` field always `None` for general type mismatches — error constructors in eval.rs always pass `None` for context, losing "which operation failed" info. Either make context mandatory and thread builtin name, or add `EvalError::with_context()` builder. (`src/error.rs:42-51`) [Major, span-integrity-checker]

### error-context: Error Context & Suggestions

Richer error context for debugging.

- [ ] Add available keys to `key_not_found` errors for "did you mean?" suggestions (use `strsim` crate for edit-distance matching)
- [ ] Filter stdlib/prelude.llt frames from user-facing stack traces (Nickel `group_by_calls` pattern)
- [ ] Build `$include` chain threading — nested include errors should show the full include path ("included from A at line X")
- [ ] Add secondary span support for "evaluated to this" labels on lazy evaluation errors (Nickel dual-position pattern)
- [ ] Reconstruct multi-hop cycle paths for circular dependency errors (show the full cycle chain, not just the blackholed thunk)

### error-ux: Error UX Features

User-facing error presentation improvements.

- [ ] Source snippets in error output — include source context with carets like rustc (span-integrity-checker review)
- [ ] Span-aware error recovery in REPL — show source line with caret pointing to error span (span-integrity-checker review)
- [ ] `tinct explain <error-code>` command for extended help on error categories (span-integrity-checker review, Elm-inspired)
- [ ] Add LSP `related_information` for materialization-site spans and stack frames (currently discarded)
- [ ] Use `ErrorKind::code()` for LSP diagnostic error code — `eval_error_to_diagnostic()` sets `code: None` instead of using the structured error code from `ErrorKind::code()`. (`src/lsp/analysis.rs:237-249`) [Minor, span-integrity-checker C32]
- [ ] Add `desugar_file()` call to LSP `DocumentState::new()` — pipeline is parse→typecheck→eval, missing the desugar step. User code containing `$_` will see un-desugared ASTs in LSP. (`src/lsp/document.rs:45-54`) [Minor, computer-scientist C32]

### error-message-polish: Error Message Polish (Minor)

Minor wording and span improvements.

- [ ] Improve document pipeline non-Dict error message for new users (`src/eval.rs:225-246`) [Minor, eval-engine]
- [ ] Fix `Span::origin()` used for non-origin errors — create separate span constructors for runtime limits and default inputs (`src/eval.rs:923, 292`) [Minor, span-integrity-checker]
- [ ] Add call-site span to depth limit errors — currently lacks stack frame attachment (`src/eval.rs:812-820`) [Minor, span-integrity-checker]
- [ ] Enhance "materialized at" error message to distinguish access vs call sites (`src/error.rs:85-86`) [Minor, span-integrity-checker]
- [ ] Change unification error wording from "type mismatch" to "cannot unify X with Y" (`src/types.rs:314`) [Minor, type-theorist]
- [ ] Improve Fn type expression error message for keyed params — currently generic (`src/typecheck.rs:764-772`) [Minor, type-theorist]

### error-infra-nits: Error Nits

Nit-level error infrastructure cleanup.

- [ ] Fix `ArityBound::Exact(1)` displaying "1 arguments" — grammatically incorrect singular; add singular/plural logic to ArityBound Display (`src/error.rs:21`) [Minor, computer-scientist]
- [ ] Fix materialize depth check message duplicating constant (`src/eval.rs:812-820`) [Nit, eval-engine]
- [ ] Simplify `EvalError::new` parameter from `impl Into<String>` to `String` (`src/error.rs:56-79`) [Nit, span-integrity-checker]
- [ ] Standardize error category names (`src/error.rs:56+`) [Nit, span-integrity-checker]
- [ ] Fix `from_json` inconsistent `.into()` usage — some error paths use `.into()` for boxing while adjacent paths use explicit `Box::new()`; standardize for consistency (`src/builtins.rs:984`) [Nit, computer-scientist]
- [ ] Review PendingBuiltin error path span handling — may overwrite operand span (`src/eval.rs:886`) [Nit, span-integrity-checker]
- [ ] Fix `checked_f64_to_i64` out-of-range branch still using `EvalError::new` — FloatNotFinite migration covered NaN/Inf but the integer range overflow path at line 110 still uses freeform `EvalError::new(format!(...))` instead of a typed ErrorKind variant (`src/builtins.rs:110`) [Nit, span-integrity-checker C31]

## stdlib-docs: Stdlib Documentation

Add type signatures and inline examples to all stdlib functions, serving as both documentation and executable tests.

- [ ] Add type annotations to all `stdlib/prelude.llt` function definitions
- [ ] Add inline assertion examples to each function (Dhall pattern: `assert` examples serve as tests AND docs)
- [ ] Generate stdlib reference documentation from annotated source
- [ ] Document `get-or`/`get-in-or` data-first argument order inconsistency — most collection functions are data-last for `->` threading but these are data-first; document rationale or provide data-last variants [Minor, stdlib-author C31]
- [ ] Document argument order convention in DESIGN.md — no clear documentation of when data-first vs data-last is appropriate (`DESIGN.md:3049-3063`) [Minor, stdlib-author C31]
- [ ] Stdlib wholeness test: single test validating entire stdlib loads and contains all expected bindings (Nickel pattern)
- [ ] Add docstrings to `$quot` and `$mod` explaining Clojure truncate-toward-zero semantics (`stdlib/prelude.llt:71-73`) [Major, stdlib-author]
- [ ] Document `map-entries` return value structure — clarify whether function receives entries and returns new values or new entries (`stdlib/prelude.llt:314`, `DESIGN.md:1513`) [Major, stdlib-author]
- [ ] Document `values` and `entries` insertion order guarantee (`stdlib/prelude.llt:180-201`) [Minor, stdlib-author]
- [ ] Mark `make-entry` as internal — add docstring or rename to `-impl` (`stdlib/prelude.llt:32`) [Minor, stdlib-author]
- [ ] Add docstring to `fold` justifying alias duplication with `reduce` (`stdlib/prelude.llt:353`) [Nit, stdlib-author]
- [ ] Document `cond` returning `[]` when no branch matches (`stdlib/prelude.llt:120-123`) [Nit, stdlib-author]
- [ ] Add 16 undocumented stdlib functions to DESIGN.md stdlib section: `const`, `>`, `<=`, `>=`, `quot`, `mod`, `ceil`, `trunc`, `join`, `words`, `nth`, `conj`, `reindex`, `from-entries`, `any?`, `all?` [Major, stdlib-author]
- [ ] Add doc comment to `Value::Seq` match arm in `value_to_json` explaining why Seq→JSON is an error and requires `$collect` first (`src/lib.rs:161-166`) [Minor, integration-verifier]
- [ ] Update DESIGN.md concat classification to note dual-dispatch: Seq path is lazy O(1), Dict path is eager O(m) (`DESIGN.md:1257`) [Minor, grammar-architect]
- [ ] Add comment to Seq cycle detection in `deep_materialize` explaining raw pointer identity pattern (`src/eval.rs:1093-1100`) [Nit, integration-verifier]
- [ ] Add 4 missing papers to DESIGN.md §Formal References — Findler & Felleisen (2002) contracts, Reynolds (1972) defunctionalization, Sestoft (1997) lazy abstract machine, Remy (1989) original row types; all cited inline in DESIGN.md but missing from the references section [Minor, computer-scientist]
- [ ] Update DESIGN.md ThunkState sketch to include `Failed(Box<EvalError>)` and `PendingCall` variants (`DESIGN.md:1988-1994`) [Nit, eval-engine]
- [ ] Delete stale concat comment block in stdlib/prelude.llt — function definition correctly removed (migrated to Rust builtin) but 3-line comment block remains as confusing dead documentation (`stdlib/prelude.llt:301-303`) [Major, stdlib-author]
- [ ] Sync DESIGN.md concat documentation after builtin migration — concat still listed as stdlib function at line 3075, non-existent `concat-seq` listed at line 3185, Sequences builtin row at line 2940 missing concat, concat Seq path marked as future work at line 5439 but already implemented. Move to builtin docs, remove stale entries, update status markers. (`DESIGN.md:2940, 3075, 3185, 5439`) [Major, integration-verifier]
- [ ] Update DESIGN.md builtin count after concat migration — line 2927 says "44 total" but `standard_builtins()` now registers 45 builtins after concat addition. Previous "verified correct" resolution (TODO.md:651) is now stale. (`DESIGN.md:2927`) [Minor, stdlib-author + integration-verifier]

## Test Infrastructure

Improvements to test infrastructure identified by cross-language analysis and test-crafter review (2026-04-19).

### test-critical: Critical Test Coverage

- [ ] PendingBuiltin state transition unit tests — verify Unevaluated→PendingBuiltin→Materialized lifecycle, error recovery, cycle detection in isolation (`src/value.rs`, `src/eval.rs`) [Critical, test-crafter]
- [ ] Add error corpus tests with span assertions — current tests check message content only, not definition_span, materialization_span, or stack frame accuracy (`tests/corpus/eval/errors/`) [Critical, test-crafter + span-integrity-checker]
- [ ] Add selective materialization unit tests — use mock/panic functions to prove unused branches stay unevaluated (`src/eval.rs`) [Critical, test-crafter]
- [ ] Expand `tests/corpus/eval/laziness/` with more negative tests proving unused expressions are NOT evaluated (current: 9 tests, target: 15+)
- [ ] Add builtins.rs unit tests for additional edge cases — NaN, overflow, Unicode, cycle detection (337 tests exist, expand for special values) (`src/builtins.rs`) [Major, test-crafter]
- [ ] Add typecheck corpus tests (currently zero; Nickel has 90+ granular typecheck test files)
- [ ] Add `deep_materialize` corpus tests through the public API
- [ ] Materialization behavior corpus tests proving stdlib laziness categories (test-crafter review)
- [ ] Add `test_type_of_seq()` unit test verifying `builtin_type_of` returns `"Seq"` for `Value::Seq` — all other Value variants have type-of tests but Seq is missing (`src/builtins.rs`) [Major, integration-verifier]
- [ ] Add sequence constructor error path corpus tests — `range_start_overflow.txt`, `iterate_non_function.txt`, `unfold_invalid_return.txt`, `cycle_empty.txt` (`tests/corpus/eval/errors/`) [Critical, test-crafter]
- [ ] Add laziness proof tests for map/filter — `map_preserves_thunks.txt`, `filter_selective_materialization.txt` proving unused values stay unevaluated (`tests/corpus/eval/laziness/`) [Critical, test-crafter]
- [ ] Add laziness materialization ORDER tests — verify left-to-right argument evaluation in builtins, predicate-before-body ordering in conditionals, dict entry insertion order preservation; current tests prove "unused = not evaluated" but not evaluation order (`tests/corpus/eval/laziness/`) [Major, test-crafter C31]
- [ ] Add parser-level unit tests for `$_` exclusion positions — verify parsed AST shows `$_` as VarRef (not desugared) in bracket key `$data[$_]`, range bounds `$data[$_..5]`, dict key `[$_: value]` positions (`src/parser.rs`) [Minor, test-crafter C31]
- [ ] Add Failed state same-span deduplication test — access Failed thunk twice with same span, verify no duplicate stack frames (`src/eval.rs`) [Minor, test-crafter]
- [ ] Add Failed state None→Some→Some edge case test — first access with None, then Some(span1), then Some(span2); verifies is_none() path (`src/eval.rs`) [Minor, test-crafter]
- [ ] Add doc comment to Failed state handler explaining dual-span model conditional update strategy (`src/eval.rs:873-894`) [Nit, span-integrity-checker + eval-engine]
- [ ] Add formatter error path tests — test `format_source()` returning `Err(LexError)` for unterminated strings, invalid escapes, bare `$`. Zero tests exist for error paths; all 48 formatter tests test successful formatting only. (`src/formatter.rs`) [Critical, test-crafter]
- [ ] Add Seq deep_materialize cycle corpus test — end-to-end corpus test for `--eval` forcing cyclic Seq structure (unit test exists at `src/eval.rs`, no corpus test) (`tests/corpus/eval/`) [Major, test-crafter + eval-engine]
- [ ] Add error corpus tests for drop/reduce/join type/arity mismatches — `drop_wrong_type.txt`, `reduce_wrong_type.txt`, `join_wrong_type.txt` (`tests/corpus/eval/errors/`) [Major, test-crafter]
- [ ] Add unit tests for builtin_drop, builtin_reduce, builtin_join (PendingCall chain construction, thunk state, span propagation) (`src/builtins.rs`) [Major, test-crafter]
- [ ] Add include caching corpus tests — same file included twice returns identical result, nested includes share cache, verify cache interaction with cycle detection (`tests/corpus/eval/builtins/`) [Major, test-crafter]
- [ ] Add concat error corpus tests — invalid input types, type mismatches (`tests/corpus/eval/errors/`) [Minor, span-integrity-checker]
- [ ] Add 4 missing concat unit tests — concat_seq (basic Seq chaining), concat_seq_empty_xs, concat_seq_empty_ys, concat_dict (Dict path eager merge). Other dual-dispatch builtins (map, filter, drop, reduce, join) have comprehensive unit test coverage for both paths. (`src/builtins.rs`) [Critical, test-crafter]
- [ ] Fix concat_large_seq corpus test label — comment claims it verifies "lazy evaluation" but actually tests collect's depth behavior (300 elements << 1M limit). Relabel to clarify it tests depth, not MAX_COLLECT_SIZE boundary. (`tests/corpus/eval/builtins/concat_large_seq.llt-eval:2-4`) [Minor, test-crafter]
- [ ] Migrate eval.rs:140-148 TypeAssertFailed to use `EvalError::type_assert_failed()` constructor — missed during migrate-d Task 1 migration sweep. Still uses verbose `Box::new(EvalError { kind: ErrorKind::TypeAssertFailed {...}, ... })`. (`src/eval.rs:140-148`) [Nit, computer-scientist panel]
- [ ] Migrate builtin_range ArityBound::Range to named constructor — uses direct struct literal for `ArityBound::Range(1, 2)` instead of named constructor. Add `arity_mismatch_range()` and `arity_mismatch_at_most()` constructors to EvalError. (`src/builtins.rs:1295-1304`, `src/error.rs`) [Nit, sprint-reviewer + computer-scientist panel]
- [ ] Expand is_cacheable/is_catchable tests to cover all 26 ErrorKind variants — currently test 7/26 and 6/26 respectively. Sufficient logically but inconsistent with the all-variants pattern used by Display and PartialEq tests. (`src/error.rs`) [Nit, computer-scientist panel]

### test-additional: Additional Test Coverage

- [ ] Fix `any?`/`all?` using `$length` for empty check — materializes entire collection (O(n)) just to check emptiness; breaks on infinite Seq (hangs). Replace with direct `$head`-based check or `$reduce` without empty guard. Also prevents Seq support since `$length` requires finite collection. (`stdlib/prelude.llt:60-78`) [Major, stdlib-author + computer-scientist]
- [ ] Add stdlib corpus tests for `from-entries`, `any?`, `all?` — functions added in Phase 4b½ lack dedicated corpus verification; short-circuit semantics for `any?`/`all?` are critical for correctness (`tests/corpus/eval/stdlib/`) [Major, stdlib-author]
- [ ] Add error corpus tests for arithmetic overflow ($+/$-/$* with i64 bounds), NaN/Infinity rejection ($floor/$round), string parse failure ($to-int/$to-float), TypeAssert failure, range mixed keys [Critical, test-crafter]
- [ ] Add depth limit corpus tests (256 levels succeeds, 257 errors)
- [ ] Add keyword-in-context corpus tests (`[call: 42]`, `[fn: hello]` testing colon-lookahead)
- [ ] Add static constraint negative tests (variadic-not-last, rest-entry position, annotation context)
- [ ] Add stack frame correctness unit tests — verify chain with correct labels and spans (`src/eval.rs:825+`) [Minor, span-integrity-checker]
- [ ] Add type system literal widening tests — widening chain, nested computed keys, polymorphic call with literals (`src/typecheck.rs:83`) [Minor, test-crafter]
- [ ] Add SPEC.md grammar coverage tests — parser_mechanisms tests for 100% grammar rule coverage (`SPEC.md`, `tests/corpus/valid/`) [Minor, test-crafter]
- [ ] Add `$_` desugared lambda type inference tests — verify inferred types of desugared expressions (e.g., `$_.name` → `Fn(Any → Any)`); current tests only validate runtime behavior, not type inference (`src/typecheck.rs`) [Minor, test-crafter C31]
- [ ] Add `$_` implicit lambda edge case tests — nested `$_`, shadowing when `_` already bound, desugaring in dict entries vs call args (`src/desugar.rs`) [Minor, test-crafter]
- [ ] Add row polymorphism tests for Closed-specific behavior — closed record with extra fields (`src/types.rs:679-837`) [Nit, type-theorist]
- [ ] Add === delimiter edge case tests — `delimiter_in_string.txt`, `delimiter_partial.txt`, `delimiter_triple_docs.txt` (`tests/corpus/valid/edge_cases/`) [Major, test-crafter]
- [ ] Add CRLF line ending corpus test — create `.txt` with actual `\r\n` bytes (`tests/corpus/valid/edge_cases/crlf_line_endings.txt`) [Minor, test-crafter]
- [ ] Add Unicode identifier corpus test — `[$café: espresso]` and other Unicode var names (`tests/corpus/valid/literals/unicode_identifiers.txt`) [Minor, test-crafter]
- [ ] Add annotated bare word corpus tests — `[x@Number: 42]`, `[fn@Int [] 42]` (`tests/corpus/valid/annotations/`) [Minor, test-crafter]
- [ ] Add variadic + named args interaction test — positional + variadic + named args together (`tests/corpus/eval/fn_variadic_plus_named.txt`) [Minor, test-crafter]
- [ ] Rename `threading.txt` test file to `pipeline.txt` to match function name (`tests/corpus/eval/stdlib/threading.txt`) [Nit, stdlib-author]
- [ ] Add TypeAssert `default:` fallback corpus test — `[@Number default: 42 "not a number"]` returns 42 (`tests/corpus/eval/builtins/`) [Minor, test-crafter]
- [ ] Add type error corpus tests directory — `type_mismatch.txt`, `unification_failure.txt`, `record_field_missing.txt` (`tests/corpus/eval/type_errors/`) [Major, test-crafter]

### test-framework: Test Framework Enhancements

- [ ] Extend error test framework: support `=== ERROR: substring` for message validation (test-crafter review)
- [ ] Generate per-file test functions for clearer failure reports (Nickel `test_resources!` pattern)
- [ ] Add snapshot testing for error messages using `insta` crate — after error format stabilizes (test-crafter review)
- [ ] Add `just coverage` command using cargo-llvm-cov for coverage measurement (test-crafter review)
- [ ] Add `tests/corpus/eval/regressions/` directory for regression tests
- [ ] Add cross-feature interaction tests (`tests/corpus/eval/cross_feature/`)
- [ ] Consider `assert_ast_eq!` macro for critical parser tests instead of Display comparison (`src/parser.rs:131`) [Minor, test-crafter]
- [ ] Rename builtin tests to `test_*` convention or document current convention (`src/builtins.rs:2298-4700`) [Nit, test-crafter]
- [ ] Standardize error corpus test format and document in README (`tests/corpus/`) [Nit, test-crafter]

### test-advanced: Advanced Testing (Fuzzing, Property-Based, Benchmarks)

- [ ] Add fuzzing targets (`fuzz/fuzz_targets/parse.rs`, `fuzz/fuzz_targets/eval_source.rs`)
- [ ] Add property-based testing (proptest) for parser round-trip and evaluator commutativity
- [ ] Add benchmarking via criterion crate — parser, evaluator, type checker baselines (performance-expert review)
- [ ] Add stack-size canary test (~200 nested brackets)
- [ ] Add pretty-print round-trip idempotence test (parse → Display → re-parse → Display → compare)
- [ ] Add `$_` formatter round-trip tests — parse code containing `$_`, format, re-parse, assert AST equality; test patterns: `$_` in call args, nested `$_`, `$_.field[0]` (`src/formatter.rs`) [Minor, test-crafter C31]
- [ ] Add function variance transitivity test or property test — transitivity assumed but not proven for subtyping (`src/types.rs:74-80`) [Major, type-theorist]

### test-tooling: Tooling Integration Tests & Documentation

- [ ] Integration tests for REPL/LSP — multi-line input, hover on nested expressions, multiple errors (test-crafter review)
- [ ] Add LSP corpus tests (`tests/lsp_corpus/`) with `.llt` + `.expected.json` per position
- [ ] IncludeContext API documentation — add docstrings to `eval_source()`, `eval_file()`, `eval_file_with_input()` warning that `$include` requires `set_include_context()` setup (`src/lib.rs`) [Major, integration-verifier]
- [ ] Document circular builtins⇄eval dependency — add safety comment at `src/builtins.rs:28` explaining the value-level vs import-level dependency [Minor, integration-verifier]
- [ ] Cross-layer contracts documentation — add §Implementation Architecture to DESIGN.md documenting pipeline phases (parse→typecheck→eval→serialize), cross-layer contracts (BuiltinFn signature, serializer requirements, thread-local state discipline), Expr→eval exhaustiveness invariant, Value→serializer coverage, type checker advisory role, environment chain construction order [Major, integration-verifier]
- [ ] Document `value_to_json` vs `value_to_display_string` NaN/Infinity difference — add test for display_string with NaN/Inf (`src/lib.rs:112-125, 176-211`) [Minor, integration-verifier]
- [ ] Add lib.rs IncludeContext doc comment mentioning cache behavior — memoizes evaluated include results, Jsonnet-style (`src/lib.rs:44-46`) [Minor, integration-verifier]
- [ ] Add DESIGN.md testing requirements section — testing philosophy and per-decision test requirements [Minor, test-crafter]

## Documentation Divergences (DESIGN.md / SPEC.md / Code)

Found by systematic comparison of DESIGN.md, SPEC.md, and source code (2026-04-18).

### SPEC.md vs Code (from REVIEW.md)

- [ ] **SPEC.md section 7 semicolon rule divergence** — SPEC.md:802-804 defines `semicolon = _{ ";" }` as standalone rule, but grammar.pest:119 uses `";"?` inline; either add rule to grammar.pest or inline in SPEC.md [Minor, grammar-architect]
- [ ] **SPEC bare_word_char prose terminator list missing `$`** — SPEC.md formal grammar (`SPEC.md:161`) is correct and matches `grammar.pest:219-225`, but the prose terminator list (`SPEC.md:168-170`) omits `$`; add `$` to the bulleted list [Nit, grammar-architect]
- [ ] **SPEC.md §3.4 access chain grammar missing dot exclusion clarity** — add inline comment showing `.` exclusion in `var_ident_char` (`SPEC.md:85-92`) [Major, grammar-architect]
- [ ] **SPEC.md annotation_value comment doesn't reference parent rule** — reference `param_annotation`/`fn_annotation` (`SPEC.md:792`) [Nit, grammar-architect]
- [ ] **SPEC.md Token Precedence missing annotated_bare** — add `annotated_bare` at position 5.5 (`SPEC.md:177-186`) [Nit, grammar-architect]
- [ ] **SPEC.md Bracket Nesting Depth Limit doesn't link to TODO.md** — add document reference (`SPEC.md:645-647`) [Nit, grammar-architect]
- [ ] **SPEC.md §8.11 has prose explanation but others don't** — be consistent across examples (`SPEC.md:1137-1139`) [Nit, grammar-architect]
- [ ] **SPEC.md Appendix numbered but only one exists** — remove "A" from "Appendix A" (`SPEC.md:1253`) [Nit, grammar-architect]
- [ ] **SPEC.md §1 Notation table missing compound-atomic** — add `${ ... }` entry (`SPEC.md:11-38`) [Nit, grammar-architect]
- [ ] **SPEC.md describes parser only, not eval semantics** — add note linking to DESIGN.md for eval semantics (`SPEC.md:1-12`) [Nit, integration-verifier]
- [ ] **SPEC.md §4 missing Value::Seq documentation** — `Value::Seq` exists in code (`src/value.rs:84-85`) and DESIGN.md covers it extensively, but SPEC.md §4.2 (Expr enum) and §4.4 (Node Semantics) don't mention Seq; add §4.5 or amend §4.4 to document runtime Seq values created by builtins [Major, integration-verifier]
- [ ] **SPEC.md §9.3 exhaustiveness claim overstated** — line 1356 claims error categories are exhaustive but omits `IntegerOverflow`, `FloatNotFinite`, `IncludeCycle`, `IncludeNotFound`, `IncludeReadError`, `Internal`; qualify claim or expand list [Major, integration-verifier]
- [ ] **SPEC.md §6.4 keyword colon lookahead only matches horizontal whitespace** — `colon_ahead` rule (`grammar.pest:76`) is `ws_chars* ~ ":"` where `ws_chars` excludes newlines; SPEC.md says "not followed by (optional whitespace then) `:`" without specifying horizontal-only. `call\n: value` parses as CallExpr not Dict. Document horizontal-only constraint. (`src/grammar.pest:70-77`, `SPEC.md:710-725`) [Major, grammar-architect]
- [ ] **SPEC.md §5.3 duplicate key detection contradicts parser** — SPEC.md:634 claims VarRef keys participate in duplicate detection (`[$k: a $k: b]` is parse error) but parser only checks literal string keys. VarRef and bracket-expr keys defer to runtime. Update SPEC.md to clarify literal-keys-only parse-time detection. (`src/parser.rs:684-724`, `SPEC.md:634`) [Major, grammar-architect]
- [ ] **SPEC.md:173 tree-sitter divergence note references nonexistent grammar** — no `grammar.js` or tree-sitter directory exists in repository. Remove or mark as "planned". (`SPEC.md:173`) [Major, grammar-architect]
- [ ] **SPEC.md §2.4 token precedence order missing structural punctuation** — omits `@` (annotation) which is structural and not a bare word character. Add note for `@`, `[`, `]`, `:`, `;` as structural punctuation. (`SPEC.md:176-186`) [Minor, grammar-architect]
- [ ] **SPEC.md §2.1 whitespace significance lacks lexer cross-reference** — hand-written lexer (`src/lexer.rs:120-129`) uses `last_significant_token` tracking for O(1) whitespace-sensitive access detection, distinct from pest's compound-atomic mechanism. Add cross-reference. (`SPEC.md:54-62`, `src/lexer.rs:120-129`) [Minor, grammar-architect]
- [ ] **SPEC.md §3.4 and §7 need dual parser/lexer implementation notes** — SPEC.md describes pest grammar as sole implementation but hand-written lexer provides alternative tokenization path. Add implementation notes to §3.4 Access Chains (distinguish compound-atomic vs last-token tracking) and §7 Complete Grammar (reference src/lexer.rs). (`SPEC.md:296-338, 774`) [Minor, grammar-architect]
- [ ] **SPEC.md §5.2 call arity checking not specified as eval-time** — reader might infer parser validates call arity; add note that arity beyond function position is eval-time per DESIGN.md §Call Convention. (`SPEC.md:619`) [Minor, grammar-architect]
- [ ] **SPEC.md missing document pipeline/$include semantics** — DESIGN.md has complete formal spec (§Document Pipeline and $include) but SPEC.md only covers `---` syntax in §3.1; add evaluation semantics for `$$` binding, `$include` cycle detection, and caching [Minor, integration-verifier]

### DESIGN.md vs Code (from REVIEW.md)

- [ ] **DESIGN.md EvalContext section misleading** — §EvalContext (DESIGN.md:2332-2378) documents the EvalContext/EvalConfig/EvalState design as current architecture, but code still uses thread-local INCLUDE_CTX. Add "**Not yet implemented — see TODO.md include-refactor**" note to section header (`DESIGN.md:2332`) [Critical, integration-verifier]
- [ ] **Dot-in-bare-word conflict across SPEC/DESIGN/tree-sitter** — pest allows `.` in bare words but tree-sitter excludes it; document divergence and rationale (`SPEC.md:161`, `DESIGN.md:856`, `tree-sitter-llt/grammar.js:9`) [Critical, grammar-architect]
- [ ] **Unrecorded tree-sitter decision: dot excluded from bare_word_char** — add confirmed decision to DESIGN.md documenting intentional divergence (`tree-sitter-llt/grammar.js:8-9`) [Critical, grammar-architect]
- [ ] **Document separator missing from DESIGN.md Tokenization Rules** — add `---` subsection (`DESIGN.md:722-813`) [Major, grammar-architect]
- [ ] **DESIGN.md Tokenization Rules tables have inconsistent headers** — unify column headers (`DESIGN.md:765-812`) [Nit, grammar-architect]
- [ ] **DESIGN.md pipeline model uses code fence for non-code** — use indentation instead (`DESIGN.md:20-29`) [Nit, grammar-architect]
- [ ] **DESIGN.md "Bare Word Character Set" header doesn't follow pattern** — retitle for consistency (`DESIGN.md:852-886`) [Nit, grammar-architect]
- [ ] **Stdlib count mismatch: 28 builtins vs 62 total not clarified** — update headers to show breakdown (`DESIGN.md:1393`, `CLAUDE.md:30`) [Nit, integration-verifier]

- [ ] **DESIGN.md laziness tables use future tense for completed builtin-thunk-return work** — change "Will return..." to past tense (`DESIGN.md:1245, 1268, 1761-1762`) [Minor, integration-verifier]
- [ ] **DESIGN.md BuiltinFn signature section omits BuiltinArgs struct** — update to mention builtin-thunk-return parameter bundling (`DESIGN.md:1732-1744`) [Minor, integration-verifier]
- [ ] **DESIGN.md include caching description sparse** — expand line 1835 to document cache key (canonical PathBuf), cache scope (thread-local), error non-caching rationale, cache lifetime (`DESIGN.md:1835`) [Minor, eval-engine]
- [ ] **CLAUDE.md IncludeContext description missing cache field** — update builtins.rs row to mention include result cache for memoization (`CLAUDE.md:30`) [Minor, integration-verifier]
- [x] ~~**DESIGN.md stale builtin count "44 total"**~~ — was verified correct at 44, now stale again after concat builtin added (now 45). New tracking item below. [Resolved then re-staled, 2026-04-22]
- [ ] **DESIGN.md builtin count "44 total" → "45 total"** — concat was moved to Rust builtin (test asserts 45); update `DESIGN.md:2927` or remove hard count and point to `standard_builtins()` test as authoritative source [Major, stdlib-author]
- [ ] **DESIGN.md stdlib reference missing `const`** — K combinator `[fn [x] [fn [y] $x]]` defined in prelude.llt:44 but not in reference table; add under Identity/Utility (`DESIGN.md:2986-3136`) [Minor, stdlib-author]
- [ ] **DESIGN.md stdlib reference missing `from-entries`** — inverse of `entries`, implemented in prelude.llt:248; add under Dict Utilities (`DESIGN.md:3050-3063`) [Minor, stdlib-author]
- [ ] **DESIGN.md stdlib reference missing `any?` and `all?`** — predicates implemented in prelude.llt:60-78; add under Logic section (`DESIGN.md:3004-3010`) [Minor, stdlib-author]
- [ ] **DESIGN.md stdlib reference missing `until`** — iterate-until-predicate, implemented in prelude.llt:154; add under Control Flow (`DESIGN.md:3042-3048`) [Minor, stdlib-author]
- [ ] **DESIGN.md `join` argument order inconsistency** — reference says `[fn [sep xs] ...]` but Rust builtin takes `(xs, sep)`; verify and fix doc or code (`DESIGN.md:3038`) [Nit, stdlib-author]
- [ ] **DESIGN.md `concat` listed in both Rust builtins and LLT List Operations** — now a Rust builtin; remove from LLT table or add migration note (`DESIGN.md:3075,2940`) [Nit, stdlib-author]
- [ ] **Include cache code comments** — add skip-guard rationale at cache hit, clarify "Check cache" comment placement, add doc comment to cache field (`src/builtins.rs:1036-1039,52`) [Nit, eval-engine]
- [ ] **IncludeContext::new() constructor** — add constructor to reduce breaking changes when fields are added; low priority pre-1.0 (`src/builtins.rs:54`) [Nit, integration-verifier]
- [ ] **DESIGN.md §Literal Recognition references "tokenizer" but should reference "lexer"** — both pest grammar (`grammar.pest`) and hand-written lexer (`src/lexer.rs`) exist; cross-reference both for precedence rules (`DESIGN.md:198-220`, `src/lexer.rs`) [Major, grammar-architect]
- [ ] **Formatter `is_fn_params` heuristic fragile** — operates on flat token stream without AST context; heuristics at `src/formatter.rs:418-450` can misfire on comments containing "fn" before brackets. Either pass AST to formatter or document best-effort nature. (`src/formatter.rs:418-450`) [Major, grammar-architect]

- [ ] **DESIGN.md missing `desugared` field documentation** — `Expr::Fn` has a `desugared: bool` origin tag (Pombrio & Krishnamurthi 2014) set by `wrap_expr_in_lambda()`, but DESIGN.md §`$_` Desugaring doesn't mention it. Add paragraph explaining origin tracking motivation and tooling use cases. (`DESIGN.md:2096-2103`, `src/ast.rs:106-110`) [Nit, grammar-architect + computer-scientist C32]

### DESIGN.md documentation gaps (eval-engine review)

- [ ] **Letrec key parent scope justification** — document in DESIGN.md why dict keys in letrec evaluation use the parent scope rather than the letrec env. Include note that effectful expressions (currently only `$include`) in computed keys execute in parent scope context. (`src/eval.rs:327`, `DESIGN.md`) [Minor, eval-engine]
- [ ] **Cycle detection recovery strategy** — document in DESIGN.md what happens after InProgress cycle detection fires: thunk state management, error propagation, and whether thunk is left in InProgress or restored (`src/value.rs`, `DESIGN.md`) [Minor, eval-engine]
- [ ] **deep_materialize visited set semantics** — visited set uses permanent insertion without removal; for 10,000-entry Dicts, HashSet grows and never shrinks. Clarify in DESIGN.md whether visited is scoped per-branch or global (currently global, matching Nix's `forceValueDeep`). (`src/eval.rs:1091-1098`) [Minor, eval-engine]
- [ ] **Materialization span semantics for PendingCall func error** — when `func_thunk` materialization fails in PendingCall handler, `call_span` is passed as mat_span. Nested errors get call_span instead of inner expression's access site. Consistent with PendingBuiltin, but DESIGN.md §Error Semantics doesn't specify mat_span semantics for nested forcing during thunk state resolution. Clarify in DESIGN.md Part 1. (`src/eval.rs:968`) [Minor, eval-engine]

### parser-docs: Parser Documentation Fixes

Found by computer-scientist codebase review (2026-04-22).

- [ ] Fix `parse_expression` docstring — incorrectly claims scope-chain correspondence; `parse_expression` discards earlier expressions entirely without evaluating them for bindings (`src/parser.rs:68`) [Minor, computer-scientist]
- [ ] Document `key_to_string` computed key duplicate detection as best-effort — returns None for `DotAccess`, `BracketAccess`, `Call` etc., so `build_dict_entries` silently skips duplicate detection for computed keys. Add code comment noting parse-time detection is literal-keys-only; computed keys checked at eval-time by `eval_dict`. (`src/parser.rs:682-691`) [Minor, computer-scientist]

### builtins-message-polish: Builtin Error Message Polish

Found by computer-scientist codebase review (2026-04-22).

- [ ] Fix `$to-float` error message for NaN/Infinity — says "cannot parse" but value was parsed successfully; issue is policy rejection of non-finite values. Change to `"to-float: \"{s}\" parses to a non-finite value (NaN/Infinity not allowed)"`. (`src/builtins.rs:741-756`) [Minor, computer-scientist]
- [ ] Add `$eq`/`$<` precision loss warning for integers > 2^53 — `9007199254740993i64 as f64` rounds to `9007199254740992.0`, producing incorrect cross-type equality. Add range check before `as f64` promotion: error if integer abs value > 2^53. Matches Jsonnet approach. Tracked in DESIGN.md §Equality P3 as known property but no runtime guard. (`src/builtins.rs:305-306`) [Minor, computer-scientist]

### misc-nits: Miscellaneous Nits

Found by computer-scientist and stdlib-author codebase reviews (2026-04-22).

- [ ] Consider `unreachable!()` in `unescape` unknown escape fallback — currently silently preserves `\q` as `\q`; grammar enforces valid escapes, so branch is dead code. `unreachable!()` would catch grammar-parser inconsistencies during development. (`src/parser.rs:903-906`) [Nit, computer-scientist]
- [ ] Add `checked_add` to `auto_index` in `eval_dict` for consistency — `builtin_append` uses `checked_add` for same kind of integer key computation; `auto_index += 1` is unchecked. Overflow is unreachable (memory exhaustion first) but inconsistent. (`src/eval.rs:331`) [Nit, computer-scientist]
- [ ] Rename `zip-seq`/`zip-dict` to `zip-seq-impl`/`zip-dict-impl` — inconsistent with other internal helpers which use `-impl` suffix (`has?-impl`, `cond-impl`, `nth-impl`, etc.) (`stdlib/prelude.llt:410,417`) [Nit, stdlib-author]
- [ ] Move `join` from Rust builtin to LLT stdlib — implementable as one-line reduce: `[fn [sep xs] [call $reduce [fn [acc x] [call $if [call $= $acc ""] [call $str $x] [call $str $acc $sep $x]]] "" $xs]]`. LLT-First Principle violation; 71 lines of Rust for what 1 line of LLT handles. Defer to Phase 10 (after dual-dispatch reduce complete). (`src/builtins.rs:1823-1894`, `stdlib/prelude.llt`) [Major, stdlib-author]
- [ ] Remove redundant `debug_assert!(depth <= MAX_PARSE_DEPTH)` — line 182 is compiled out in release builds; the `if depth >= MAX_PARSE_DEPTH` on line 183 is the actual runtime check. (`src/parser.rs:182-183`) [Nit, grammar-architect]
- [ ] Rename error corpus test `quot_div_by_zero.llt-eval` to `division_by_zero.llt-eval` — inconsistent with `ErrorKind::DivisionByZero` and other test naming (`tests/corpus/eval/errors/`) [Nit, test-crafter]
- [ ] Clarify SPEC.md §3.5 semicolons — says `;` is "equivalent to whitespace" but it's actually an optional entry separator; reword to "optional entry separator for multiple entries on one line" (`SPEC.md:368-372`) [Nit, grammar-architect]
- [ ] Clarify SPEC.md §6.2 auto-indexing — titled as "desugaring" but is actually eval-time key assignment, not AST rewrite. Only §6.5 (`$_` desugaring) is true AST transformation. (`SPEC.md:669-697`) [Nit, grammar-architect]
- [ ] Fix stale line range in `src/desugar.rs:7` docstring — references DESIGN.md lines 1993-2126 which shifted after recent edits [Nit, grammar-architect C31]
- [ ] Fix `grammar.pest:8` comment capitalization inconsistency — uses different convention than other comments [Nit, grammar-architect C31]
- [ ] Standardize "Inherently materializing" comment annotations in `stdlib/prelude.llt` — some materializing functions have this comment, others don't; either add to all or remove and document in DESIGN.md [Nit, stdlib-author C31]

## Future Features

Deferred features moved from DESIGN.md. Evaluate when triggered.

- [x] Research pattern matching — see doc/whatif/pattern-matching.md. Recommends [match] special form (Approach A) with 5-phase adoption: type predicates → basic match → dict/seq destructuring → guards/or-patterns → exhaustiveness checking. Nickel is the only lazy config lang with full PM; Nix/Jsonnet skip it
- [x] Research parameterized type aliases — see doc/whatif/parameterized-type-aliases.md. Recommends hybrid approach (C): keep current non-parameterized behavior, add explicit `[type [a b] body]` syntax for aliases that need fresh instantiation. Deferred until variable name collision becomes a real problem
- [x] Research `let` binding form — see doc/whatif/let-binding.md. Recommends sequential expressions in function bodies (Approach B): extend existing document-level sequential scoping to work inside `[fn ...]` bodies. No new keywords, consistent mental model, enables pattern matching arm bodies
- [x] Research quasiquoting — see doc/whatif/quasiquoting.md. Recommends quote/unquote/unquote-splice as special form keywords (Approach A), phased with macro system. AST-as-dict schema mirrors Expr enum. Gated on macro system adoption
- [x] Research custom call aliases — see doc/whatif/call-aliases.md. Recommends custom call forms via procedural macros (Approach C): `call` remains the only built-in form (Principle 3), but macros can define `[timed $f ...]`, `[traced $f ...]`, etc. that expand to `call` at compile time. Gated on macro system
- [x] Research gradual typing — see doc/whatif/gradual-typing.md. Three-phase adoption (formalize → split Any → blame tracking). Gated on Any-as-top-and-bottom causing a real soundness bug or union types/type classes forcing the split
- [x] Research `list?` vs `dict?` predicates — see doc/whatif/type-predicates.md. Recommends type predicates for Value variants ($int?, $dict?, etc.) WITHOUT $list? (lists are dicts, Principle 1). Part of pattern matching Phase 1
- [x] Research string interpolation — see doc/whatif/string-interpolation.md. Recommends `i"..."` prefix syntax (Approach B): `i"Hello $name"` desugars to `[call $str ...]`. 3-phase: simple `$identifier` → dot access → expression interpolation `${...}`. Key enabler for formatter ergonomics
- [x] Research float dict keys — see doc/whatif/float-dict-keys.md. Recommends Decimal keys alongside Decimal type (Approach B): IEEE 754 floats remain unsound as keys, but Decimal provides exact base-10 arithmetic where `0.1+0.2==0.3`. 2-phase: Decimal type → Decimal keys. Gated on Decimal type adoption
- [x] Research width-specific numeric types — see doc/whatif/numeric-types.md. Recommends range contracts with automatic internal representation sizing (Approach B): `@[min: 0 max: 65535]` → runtime chooses u16 internally. 4-phase: range annotations (validation only) → Decimal type → auto representation sizing → BigInt. Ada range types as precedent
- [x] Research typeclasses — see doc/whatif/typeclasses.md. Two-phase adoption (constrained type vars → full Haskell-style classes). Gated on Any typing for dual-dispatch causing false positives or user-defined types needing protocols
- [x] Research union types — see doc/whatif/union-types.md. Three-phase path: type classes → annotation-only unions → inferred unions via Simple-sub. Gated on nullable types or tagged union patterns becoming common
- [x] Research algebraic subtyping — see doc/whatif/algebraic-subtypes.md. Simple-sub (Parreaux 2020) replacement for [U-SUBSUME] + Robinson. 4-step migration path. Gated on union types being insufficient without inferred unions or Any-as-top-and-bottom causing soundness problems
- [x] Research macros — see doc/whatif/macros.md. Recommends procedural AST macros (Approach B). Laziness reduces need; gated on second syntactic desugaring or user-requested domain-specific syntax
- [x] Research templating — see doc/whatif/templating.md. Three-part design: (1) data-first formatters (`$emit`, multi-file pipeline, stdlib/fmt/), (2) literate tinct (code blocks in Markdown, tangle/weave/eval), (3) template-polarity embedding (Jinja-style, deferred Phase 5). tinct's bracket syntax creates friction in template delimiters; `i"..."` + formatters + literate mode cover the design space without template embedding
- [x] Research structural contracts — see doc/whatif/structural-contracts.md. Hybrid: `$$@Type` for static pipeline boundary checking + `$validate` schema-as-dict for runtime constraints. 4-phase: $$@Type → $validate → tinct describe → pipeline blame
- [x] Research implied `call` — see doc/whatif/implied-call.md. Head-position `$` heuristic: if first unkeyed element is a `$`-reference, treat `[]` as a call. `call` remains valid (backwards compatible). Requires `seq` keyword for list-of-references. Critically depends on `$` sigil — incompatible with bare-word references in simplest form
- [x] Research bare-word references — see doc/whatif/bare-word-references.md. Nix/Jsonnet model: bare words in value position are references, keys stay as strings, strings must be quoted. Removes `$` sigil. Significant config ergonomic regression (must quote all strings). 4-phase adoption with dual-mode parser. Must be coordinated with implied call
