# CSV Formatter Testing Note

The CSV output formatter (`stdlib/cli/out/csv.llt`) **cannot be tested via corpus tests** because:

1. **Corpus tests capture return values, not stdout**
   - CSV formatter writes to `%stdout` via `[write-handle %stdout ...]`
   - Formatter's return value is `[await drain]` (a Task), not the CSV text
   - Corpus test framework checks the eval result, which is the Task, not the CSV output

2. **Formatter depends on CLI-provided special variables**
   - `%` — the program's return value (provided by CLI)
   - `%emit-channel` — channel for streaming values (created by CLI)
   - `%stdout` — handle to stdout (created by CLI)
   - These do not exist in corpus test evaluation context

3. **Helper functions are private**
   - `csv-quote`, `csv-header`, `csv-row` are in a local dict
   - Cannot be called directly from corpus tests

## Testing Strategy

**CLI Integration Tests** (tests/integration_csv.rs):
- Comprehensive edge case coverage
- Tests actual CSV output via stdout capture
- Verifies quoting, escaping, missing keys, column order
- Tests with special characters, unicode, empty values
- 20+ test cases covering all CSV-specific behavior

**Existing CLI Tests** (tests/cli_tests.rs):
- `output_flag_csv` — basic list-of-dicts
- `output_flag_csv_exact` — exact format verification
- `output_flag_csv_empty_input` — empty input handling

## Why Not Unit Tests in csv.llt?

Tinct stdlib files cannot contain inline tests (no `#[test]` equivalent). The formatter is a sequential-document program that executes top-to-bottom. Helper functions could theoretically be exported and tested separately, but:

1. Helper functions use recursive tail-call patterns that need full evaluation context
2. Testing helpers in isolation wouldn't verify the formatter's main logic (task coordination, oneshot channel TOCTOU fix)
3. Integration tests provide better coverage by testing the complete formatter behavior

## Coverage

All CSV formatter requirements from T-849 are covered:
- ✅ Simple dict → header + data row
- ✅ Seq of dicts → header + multiple data rows  
- ✅ Missing keys → empty cells
- ✅ Values with quotes/commas/newlines → proper escaping
- ✅ Column order from first record
- ✅ All value types (int, float, bool, string, unicode)
- ✅ Edge cases (empty strings, single column, many rows, numeric keys)
