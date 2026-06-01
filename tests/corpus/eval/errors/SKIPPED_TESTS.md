# Skipped Corpus Tests

## FloatNotFinite via Overflow (E033)

**File**: `float_overflow_add.llt-eval` (deleted)

**Reason**: LLT has no scientific notation literals. The token sequence `1e308` is lexed as
`Int(1)` followed by `Identifier("e308")` — not a float literal — so `[+ 1e308 1e308]` does not
compile to a float addition at all. There is no way to express a float overflow directly in LLT
source.

**Alternative attempted**: `[to-float "1e308"]` converts the string to a finite `f64` value
(1×10^308 is representable in IEEE 754 double precision), not an overflow. The only values that
produce E033 (FloatNotFinite) would require arithmetic producing Infinity (e.g. Inf + Inf), but
Infinity itself is also not a literal. This error path is covered by `add_float_overflow_to_infinity_is_error` (`src/builtins.rs:6953`) and related tests (`mul_float_overflow_to_infinity_is_error`, `sub_float_nan_is_error`) that call the arithmetic builtins directly with values that produce Infinity or NaN and assert the E033 error is returned.

## Include Cycle (E052)

**File**: `include_cycle.llt-eval` (not created)

**Reason**: Self-referencing include detection requires filesystem support and would need either:
1. A temporary file system setup in the test harness
2. Two separate test files where A includes B and B includes A

The corpus test format doesn't support multi-file scenarios, and the `# no_fs` directive disables all include functionality.

**Alternative**: Covered by `test_eval_error_include_cycle_constructor` in `src/error.rs` (constructor-level unit test) and the runtime include-cycle detection path in `src/builtins.rs:1187` (`EvalError::include_cycle` call in `builtin_include`).

## Parse Depth 256 Success (Task 7)

**File**: `tests/corpus/valid/edge_cases/parse_depth_256_succeeds.llt-eval` (created)

**Status**: This test now exists. 256 levels of `[` are written on a single line, making the file compact and fast to parse. It verifies that exactly MAX_PARSE_DEPTH levels of nesting succeeds, exercising the boundary just before the depth-exceeded error fires.

**Additional coverage**: A unit test in `src/parser.rs` covers the same boundary programmatically.

## cap-identity Builtin

**File**: `cap_identity.llt-eval` (not created)

**Reason**: `cap-identity` returns a `"dev:ino"` string from `fstat` on a DirCap's O_DIRECTORY fd. Exercising it requires a real DirCap, which the corpus test harness (with `# no_fs`) does not provide.

**Alternative**: The builtin is registered via `core_builtins()` / `builtin_module("core")` and its implementation is covered indirectly by the `include-cache-get`/`include-cache-put` tests which depend on it for cache keying.

## Include errors E050, E051, E053, E054

**Status**: Partially covered

- **E042 (IncludeForbidden)**: This error code no longer exists. The `ErrorKind` variant was
  removed during the include-decomp-prelude sprint when `builtin_include` was deleted. The error
  code range E040–E049 now contains only E040 (DepthExceeded), E043 (ResourceLimitExceeded), and
  E044 (CapabilityRequired). The file `include_forbidden.llt-eval` was repurposed to test E055
  (IncludeHashMismatch) and no longer tests any E042 path. No corpus test is needed or possible
  for E042 because the variant is absent from `src/error.rs`.
- **E050 (IncludeNotAvailable)**: Cannot distinguish from other include errors in corpus tests
  with `# no_fs` directive
- **E051 (IncludeIoError)**: Would require actual filesystem errors (permission denied, etc.)
- **E052 (IncludeCycle)**: See "Include Cycle" above
- **E053 (IncludeParseFailed)**: Would require a malformed .llt file to include
- **E054 (IncludeFileTooLarge)**: Would require a file larger than MAX_FILE_SIZE (10MB)

These are better tested as integration tests or unit tests with mocked filesystem operations.

## BuilderFinished (E082)

**File**: `builder_set_after_finish.llt-eval` (not created)

**Reason**: E082 (BuilderFinished) cannot be triggered from an LLT corpus test. The error is
raised in seven locations in `src/builtins_dict.rs` — by `builder-set` (line 1116),
`builder-delete` (line 1186), `builder-finish` (line 1230), `builder-snapshot` (line 1274),
`builder-has?` (line 1343), `builder-get` (line 1415), and `builder-get-or` (line 800) — all
when operating on a builder that has already been finished via `builder-finish`.

The obstacle is structural: in LLT's lazy CEK evaluator, the result of a post-finish operation
on a shared builder thunk surfaces as E035 (ValueNotSerializable) rather than E082. When
`builder-finish b` is called and the builder is later referenced again via the same thunk `b`,
the `Value::Builder(Arc<Builder>)` value stored in the memoized thunk is returned by
`try_get_materialized`. Even though the underlying `Arc<Builder>` now has `frozen=true`, the
evaluator path for mutations (e.g. `builder-set`) returns `Ok(Arc::clone(&args[2]))` — the
original builder thunk — only after the `builder.set()` call fails. That failure produces E082
internally, but the returned value, a `Value::Builder`, is then subject to JSON serialization and
produces E035 instead. The existing `tests/corpus/eval/builtins/builder_frozen_error.llt-eval`
demonstrates this behavior: it asserts `[E035]` rather than `[E082]` for a `builder-set` on a
finished builder.

**Alternative**: The `test_error_code_exhaustiveness` and `test_error_kind_display_all_variants`
unit tests in `src/error.rs` verify the E082 code string and display format via direct
`ErrorKind::BuilderFinished { op: "set".to_string() }` construction (lines ~2522, ~4462).

**Re-evaluate when**: The builder builtins are refactored to return the error result directly
(rather than returning the builder thunk), making the E082 path observable before JSON
serialization.

## DuplicateVariable (E072)

**File**: `duplicate_pattern_variable_error.llt-eval` (not created)

**Reason**: E072 (DuplicateVariable) cannot be triggered from LLT surface syntax. The
`check_pattern_linearity` function at `src/eval.rs:3094` that raises E072 is annotated
`#[cfg(test)]` — it is a test helper only and is not called from any production evaluation path.
The doc comment at line 3089 explains: "Production code uses last-binding-wins semantics for
non-linear patterns (see doc/14-patterns.md §Non-Linear Patterns). This function is retained as
a test helper to verify duplicate-detection logic."

In practice, a pattern like `[a: x  b: x  ...]:` with two bindings for `x` is legal in LLT and
binds `x` to the last matched value rather than erroring. The `EvalError::DuplicateVariable`
variant exists for potential future enforcement of pattern linearity, but no evaluator path
currently raises it from user-provided LLT source.

**Alternative**: The `test_check_pattern_linearity_duplicate_in_dict_rejected` and
`test_check_pattern_linearity_duplicate_in_seq_rejected` unit tests in `src/eval.rs` (lines
~8802, ~8828) verify the E072 constructor and display format by calling
`check_pattern_linearity` directly with synthetic patterns.

**Re-evaluate when**: Pattern linearity is enforced in the production evaluator (i.e., when
`check_pattern_linearity` is called from `match_pattern` or the desugarer rather than only from
test code).

## KindMismatch (E091)

**File**: `kind_mismatch.llt-eval` (not created)

**Reason**: `EvalError::kind_mismatch` (the runtime E091 constructor in `src/error.rs:1348`) has zero
call sites in production code. Kind-mismatch errors raised during type-checking use `TypeError::kind_mismatch`
(in `src/type_env.rs:1499`) and produce T091 type errors, not E091 eval errors. The `EvalError::KindMismatch`
variant and its constructor exist for future HKT runtime enforcement, but no evaluator path currently
raises it.

**Alternative**: Covered by the `test_error_kind_display_all_variants` and
`test_error_code_exhaustiveness` unit tests in `src/error.rs`, which verify the E091 code and display
format without requiring a triggering LLT expression. Type-level kind-mismatch errors are covered by
the HKT corpus tests in `tests/corpus/eval/errors/hkt_do_inferred_maybe.llt-eval` and
`tests/corpus/eval/errors/hkt_do_inferred_unresolvable.llt-eval` (T091 type errors).

**Re-evaluate when**: A new builtin or evaluator path calls `EvalError::kind_mismatch` directly.

## MacroError (E012)

**File**: `macro_error_direct.llt-eval` (not created)

**Reason**: `EvalError::macro_error` (E012) is raised at `src/expand.rs:531`, `src/expand.rs:951`, and `src/builtins_meta.rs:247`. The `builtin-macro-error` builtin is registered in `core_builtins()` but is not present in `core_type_env()`, so a corpus test using `--- uses: ["core"]` still produces a T002 typecheck warning (undefined variable) and the corpus runner rejects the test. The higher-level `macro-error` prelude export requires a span dict produced by `span-of`, which itself requires an AST Expression node — a value only available inside a macro body, not in regular evaluation.

**Alternative**: Covered by `test_error_kind_display_all_variants` and `test_error_code_exhaustiveness` unit tests in `src/error.rs`, which verify the E012 code string and display format. The macro expansion error path is also exercised end-to-end by `tests/corpus/eval/errors/macro_named_arg_error.llt-eval` and `tests/corpus/eval/errors/macro_returns_wrong_type.llt-eval` (via the `tmpl` macro), which trigger E080 wrapping an internal MacroError.

**Re-evaluate when**: `builtin-macro-error` is added to `core_type_env()`, making it accessible via `--- uses: ["core"]` in corpus tests without a T002 warning.
