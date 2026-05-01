# Test Corpus

Organized test suite for the tinct parser.

## Directory Structure

```
tests/corpus/
├── valid/                # Inputs that should parse successfully
│   ├── simple/           # Basic key-value pairs and simple structures
│   ├── complex/          # Nested structures, metadata, larger examples
│   ├── literals/         # Int, float, bool, string, bare word literals
│   ├── special_forms/    # call, fn, type special forms
│   ├── access/           # Dot, bracket, range access
│   ├── annotations/      # Type annotations and assertions
│   ├── documents/        # Multi-expression, multi-document, --- separator
│   └── edge_cases/       # Empty inputs, whitespace, deep nesting, CRLF
├── invalid/              # Inputs that should fail to parse
│   ├── syntax_errors/    # Bracket mismatches, unexpected tokens, depth exceeded
│   └── semantic_errors/  # Type errors, constraint violations
└── eval/                 # Evaluator tests (input === expected JSON output)
    ├── builtins/         # Builtin function evaluation
    ├── stdlib/           # Stdlib function evaluation
    ├── errors/           # Expected eval failures (input === ERROR)
    ├── laziness/         # Laziness proof tests (use $error in unused positions)
    ├── type_assertions/  # TypeAssert structural contract evaluation
    ├── type_errors/      # Type checker error tests
    ├── typecheck/        # Type inference and checking tests
    ├── functions/        # Function definition and call tests
    ├── documents/        # Multi-document evaluation and pipelines
    ├── access/           # Access chain evaluation
    ├── underscore/       # $_ implicit lambda desugaring
    └── letrec/           # Letrec scoping and forward references
```

## Adding Tests

### Valid Test Cases

Create a `.llt-eval` file in the appropriate `valid/` subdirectory:

```bash
# Simple feature test
echo "[key: value]" > tests/corpus/valid/simple/my_test.llt-eval

# Complex nested test
cat > tests/corpus/valid/complex/my_nested.llt-eval <<'EOF'
[
    outer: [
        inner: value
    ]
]
EOF
```

### Invalid Test Cases

Create a `.llt-eval` file with malformed input:

```bash
echo "[unclosed: bracket" > tests/corpus/invalid/syntax_errors/my_error.llt-eval
```

## Test File Format

Test files are plain `.llt-eval` files containing LLT input. To specify expected output,
use a `===` delimiter: everything before the delimiter is the input, and everything
after is the expected parse result. (`===` is used instead of `---` because `---` is
a valid LLT document separator.)

```
[name: Alice]
===
Dict({"name": String("Alice")})
```

### Error Tests

For tests in `tests/corpus/eval/errors/` or `tests/corpus/invalid/`, the expected output
after `===` should be an error substring or error code. **All error tests MUST include
the error code `[EXXX]` to ensure error code stability.**

```
[call $+ 1 2 3]
===
[E020] arity mismatch
```

Error tests match on substrings, so you can specify just the error code, or include
additional message text for clarity. The test runner verifies that the error message
contains the expected substring.

If no `===` delimiter is present, the test only checks that the input parses
successfully (for valid tests) or fails (for invalid tests).

## Running Tests

```bash
# Run all tests (including corpus)
just test

# Run only corpus tests
just test-corpus

# Run specific corpus category
cargo test --test corpus_tests test_valid_corpus
cargo test --test corpus_tests test_invalid_corpus
```

## Test Output

Corpus tests show which files they're processing:

```
Testing valid input: tests/corpus/valid/simple/basic_key_value.llt-eval
✅ All 8 valid corpus tests passed
```

Failed tests show the filename and error:

```
❌ 1 valid test(s) failed to parse:
  - tests/corpus/valid/complex/foo.llt-eval: Error at line 2, column 10:
```

## Current Test Coverage

### Valid Tests (Parser)
- **simple/**: Basic key-value, multi-value, nested lists
- **complex/**: Metadata structures, deep nesting
- **literals/**: Int, float, bool, string, bare word, variable references
- **special_forms/**: call, fn, type forms
- **access/**: Dot, bracket, range, chained access
- **annotations/**: Type assertions with and without defaults
- **documents/**: Multi-expression files, multi-document pipelines
- **edge_cases/**: Empty input, whitespace, CRLF line endings, deep nesting (200 levels), delimiter edge cases

### Invalid Tests (Parse Failures)
- **syntax_errors/**: Missing brackets, extra tokens, unexpected colons, missing values, depth exceeded (257+ levels)
- **semantic_errors/**: Type errors, constraint violations

### Eval Tests (Evaluator)
- **builtins/**: All 44 builtin functions with edge cases
- **stdlib/**: All 51 stdlib functions (defined in stdlib/prelude.llt)
- **errors/**: Expected eval failures with error codes (E001-E099)
- **laziness/**: Tests proving values are NOT eagerly evaluated
- **type_assertions/**: TypeAssert contract validation with defaults and fallbacks
- **type_errors/**: Type checker error tests
- **typecheck/**: Type inference, polymorphism, let-generalization
- **functions/**: Function definitions, closures, variadic, named args
- **documents/**: Multi-document evaluation, pipelines with $$
- **access/**: Access chain evaluation
- **underscore/**: $_ implicit lambda desugaring
- **letrec/**: Letrec scoping and forward references

### Known Issues
- None currently tracked
