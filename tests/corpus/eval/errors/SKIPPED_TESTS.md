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

## Include errors E050, E051, E053, E054

**Status**: Partially covered

- **E042 (IncludeForbidden)**: ✅ Tested in `include_not_available.llt-eval` and existing `include_forbidden.llt-eval`
- **E050 (IncludeNotAvailable)**: Cannot distinguish from E042 in corpus tests with `# no_fs` directive
- **E051 (IncludeIoError)**: Would require actual filesystem errors (permission denied, etc.)
- **E052 (IncludeCycle)**: See "Include Cycle" above
- **E053 (IncludeParseFailed)**: Would require a malformed .llt file to include
- **E054 (IncludeFileTooLarge)**: Would require a file larger than MAX_FILE_SIZE (10MB)

These are better tested as integration tests or unit tests with mocked filesystem operations.
