# Structural Contracts for Pipeline Input Types - Implementation Summary

## Overview
Implemented runtime validation of pipeline input types using `%@Type` and `expects: @Type` annotations in section headers.

## Changes Made

### 1. Parser (`src/parser.rs`)
- **Added support for `%@Type` syntax** in section headers (lines 2971-3043)
  - Bare `%` followed by `@Type` is now valid and equivalent to `expects: @Type`
  - `%@Type` validates the pipeline INPUT (the `%` value from previous document)
  - `%name@Type` validates the OUTPUT (the document's result)
  - Prevents duplicate input annotations (both `%@Type` and `expects: @Type`)

### 2. Evaluator (`src/eval.rs`)
- **Added `wrap_with_nominal_validation()` function** (lines 1211-1254)
  - Creates a synthetic TypeAssert expression to wrap pipeline input
  - Uses existing TypeAssert mechanism for validation
  - Defers validation until the thunk is materialized (lazy)
  
- **Modified `eval_file_with_input()`** (lines 1267-1289)
  - When a document has an `expects` annotation, wraps the `%` binding in a validation thunk
  - Reuses existing TypeAssert evaluation logic for consistency

### 3. Tests (`tests/corpus/eval/`)
Created 4 test files to verify the implementation:

1. **`pipeline/input_validation_expects.llt-eval`**
   - Tests `expects: @Dict` annotation
   - Validates that pipeline input matches expected type
   
2. **`pipeline/input_validation_percent_at.llt-eval`**
   - Tests new `%@Type` syntax
   - Equivalent to `expects:` but more concise
   
3. **`errors/input_validation_mismatch.llt-eval`**
   - Tests type mismatch error reporting
   - Expects error message: "expected Dict, got Int"
   
4. **`pipeline/input_validation_record_type.llt-eval`**
   - Tests record type annotations
   - Validates structural types (currently nominal matching)

## Implementation Strategy

The implementation follows the TODO specification's approach:

> Parser produces a standard `Expr::TypeAssert { expr: VarRef("%"), annotation: T }` for `%@Type`

We create a synthetic TypeAssert expression that wraps a variable reference to `%_input`, which is bound to the previous document's output. This reuses the existing TypeAssert/Guarded mechanism without requiring new AST nodes or eval handlers.

## Validation Semantics

- **Lazy validation**: Type checking happens when `%` is first accessed/materialized, not at the `---` boundary
- **Nominal type checking**: Uses string comparison of type names (like `--no-typecheck` mode)
  - `@Int` checks for Int value
  - `@String` checks for String value
  - `@Dict` checks for Dict value
  - `@Number` accepts both Int and Float
- **Error reporting**: Uses existing TypeAssert error messages with dual-span information

## Compatibility

- **Type checker**: Already validates `expects:` annotations (advisory mode)
- **Formatter**: Already handles `expects:` in section headers; `%@Type` is stored in the same field
- **LSP**: No changes needed; `expects` field already exists in Document AST

## Future Enhancements

As noted in the TODO, future work includes:

1. **Structural validation**: Full record type checking with field-level validation (requires type elaboration at runtime)
2. **`validate` builtin**: Schema-as-dict validation with rich constraints (Phase 2)
3. **Pipeline blame tracking**: Attribute violations to producing stage (Phase 4)
4. **`tinct describe`**: CLI command to inspect contracts (Phase 3)

## Testing

Run the test suite with:
```bash
just test
```

The new tests are in:
- `tests/corpus/eval/pipeline/` - successful validation cases
- `tests/corpus/eval/errors/` - error case (type mismatch)
