# Skipped Corpus Tests

## Include Cycle (E052)

**File**: `include_cycle.llt-eval` (not created)

**Reason**: Self-referencing include detection requires filesystem support and would need either:
1. A temporary file system setup in the test harness
2. Two separate test files where A includes B and B includes A

The corpus test format doesn't support multi-file scenarios. This error is already tested in unit tests (see `src/error.rs:3475`).

**Alternative**: Consider adding this as an integration test in `tests/cli_tests.rs` with actual temp files.

## Parse Depth 256 Success (Task 7)

**File**: `tests/corpus/valid/edge_cases/parse_depth_256_succeeds.llt-eval` (not created)

**Reason**: A test with exactly 256 levels of nesting would be enormous (thousands of lines of `[` followed by thousands of `]`). The test file would be unwieldy and slow to parse.

**Alternative**: This is better tested as a unit test (already exists in `src/parser.rs`) where depth can be precisely controlled programmatically.

## Include errors E050, E051, E053, E054

**Status**: Partially covered

- **E042 (IncludeForbidden)**: ✅ Tested in `include_not_available.llt-eval` and existing `include_forbidden.llt-eval`
- **E050 (IncludeNotAvailable)**: Cannot distinguish from E042 in corpus tests with `# no_fs` directive
- **E051 (IncludeIoError)**: Would require actual filesystem errors (permission denied, etc.)
- **E052 (IncludeCycle)**: See "Include Cycle" above
- **E053 (IncludeParseFailed)**: Would require a malformed .llt file to include
- **E054 (IncludeFileTooLarge)**: Would require a file larger than MAX_FILE_SIZE (10MB)

These are better tested as integration tests or unit tests with mocked filesystem operations.
