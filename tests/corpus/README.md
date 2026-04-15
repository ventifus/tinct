# Test Corpus

Organized test suite for the Lazy Lisp Transformer parser.

## Directory Structure

```
tests/corpus/
├── valid/           # Inputs that should parse successfully
│   ├── simple/      # Basic key-value pairs and simple structures
│   ├── complex/     # Nested structures, metadata, larger examples
│   └── edge_cases/  # Empty inputs, whitespace handling, etc.
└── invalid/         # Inputs that should fail to parse
    ├── syntax_errors/    # Bracket mismatches, unexpected tokens
    └── semantic_errors/  # Future: Type errors, constraint violations
```

## Adding Tests

### Valid Test Cases

Create a `.txt` file in the appropriate `valid/` subdirectory:

```bash
# Simple feature test
echo "[key: value]" > tests/corpus/valid/simple/my_test.txt

# Complex nested test
cat > tests/corpus/valid/complex/my_nested.txt <<'EOF'
[
    outer: [
        inner: value
    ]
]
EOF
```

### Invalid Test Cases

Create a `.txt` file with malformed input:

```bash
echo "[unclosed: bracket" > tests/corpus/invalid/syntax_errors/my_error.txt
```

## Test File Format

Test files are plain `.txt` files containing LLT input. To specify expected output,
use a `===` delimiter: everything before the delimiter is the input, and everything
after is the expected parse result. (`===` is used instead of `---` because `---` is
a valid LLT document separator.)

```
[name: Alice]
===
Dict([Entry { key: "name", value: "Alice" }])
```

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
Testing valid input: tests/corpus/valid/simple/basic_key_value.txt
✅ All 8 valid corpus tests passed
```

Failed tests show the filename and error:

```
❌ 1 valid test(s) failed to parse:
  - tests/corpus/valid/complex/foo.txt: Error at line 2, column 10:
```

## Current Test Coverage

### Valid Tests
- **simple/**: Basic key-value, multi-value, nested lists
- **complex/**: Metadata structures, deep nesting
- **edge_cases/**: Empty input, whitespace normalization

### Invalid Tests
- **syntax_errors/**: Missing brackets, extra tokens, unexpected colons, missing values

### Known Issues
- None currently tracked
