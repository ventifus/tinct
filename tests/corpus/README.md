# Test Corpus

Organized test suite for the tinct parser.

## Directory Structure

```
tests/corpus/
├── valid/                    # Inputs that should parse successfully (with expected AST)
│   ├── access/               # Dot, bracket, range access parsing
│   ├── annotations/          # Type annotations and assertions
│   ├── complex/              # Nested structures, metadata, larger examples
│   ├── documents/            # Multi-expression, multi-document, --- separator
│   ├── edge_cases/           # Empty inputs, whitespace, deep nesting, CRLF
│   ├── literals/             # Int, float, bool, string, bare word literals
│   ├── parser_mechanisms/    # Parser-specific features (annotations, delimiters)
│   ├── simple/               # Basic key-value pairs and simple structures
│   └── special_forms/        # call, fn, type special forms
├── invalid/                  # Inputs that should fail to parse (with expected error)
│   ├── pipeline/             # Pipeline syntax errors
│   ├── semantic_errors/      # Type errors, constraint violations
│   └── syntax_errors/        # Bracket mismatches, unexpected tokens, depth exceeded
├── eval/                     # Evaluator tests (input + labeled output sections)
│   ├── access/               # Access chain evaluation
│   ├── builtins/             # Builtin function evaluation (44+ builtins)
│   ├── cross_feature/        # Multi-feature interaction tests
│   ├── errors/               # Expected eval failures (must include [EXXX] error code)
│   ├── functions/            # Function definition, call, closure tests
│   ├── laziness/             # Laziness proof tests (use $error in unused positions)
│   ├── letrec/               # Letrec scoping and forward references
│   ├── pipeline/             # Pipeline (%) evaluation tests
│   ├── regressions/          # Regression tests for specific bugs
│   ├── stdlib/               # Stdlib function evaluation (51+ stdlib functions)
│   ├── type_assertions/      # TypeAssert structural contract evaluation
│   ├── type_errors/          # Type checker error tests (expected typecheck failures)
│   ├── type_system/          # Type system evaluation tests (row polymorphism, etc.)
│   ├── typeassert/           # TypeAssert edge cases and desugaring
│   ├── typecheck/            # Type inference and checking tests (expected success)
│   └── underscore/           # $_ implicit lambda desugaring
└── typecheck/                # Typecheck-specific tests
    └── warnings/             # One seed test per type warning category
```

### Required Directories

All directories listed above MUST exist. The test suite validates directory structure
in `test_corpus_structure()`. Missing directories will cause test failures.

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

All corpus test files use the `.llt-eval` extension. Test files contain LLT source code
followed by zero or more **labeled sections** that define expected outputs.

**Section format:** `\n=== label\nexpected_content`

Valid labels: `out`, `warn`, `error`. Bare `===` (without a label) is a **parse error** —
the test runner panics with a clear message. (`===` is used instead of `---` because `---`
is a valid LLT document separator.)

See `doc/12-tooling.md §Corpus Test Format` for the authoritative reference.

### Labeled Sections

| Section | Content | Assertion |
|---------|---------|-----------|
| `=== out` | Expected eval output | Exact match against `eval_source()` result |
| `=== warn` | Expected type warning substring | `typecheck_source()` must return `Err` containing this |
| `=== error` | Expected error substring | Eval must fail; error message must contain this |

**Zero-warning enforcement:** A test file with **no `=== warn` section** asserts that
`typecheck_source()` returns `Ok(())` — zero type warnings. If a file produces unexpected
type warnings, either add an explicit `=== warn` section or fix the type checker issue.

Sections may appear in any order. Multiple sections are allowed.

### Valid Corpus Tests

For tests in `tests/corpus/valid/`, the `=== out` section is the AST Display format:

```
[name: Alice age: 30]
=== out
["name": Alice  "age": 30]
```

### Eval Corpus Tests

For tests in `tests/corpus/eval/` (excluding `errors/` and `type_errors/`):

```
[call $+ 1 2]
=== out
3
```

A test that produces a known type warning documents it explicitly:

```
[call $+ "hello" 42]
=== out
[E020] arity mismatch
=== warn
cannot unify String with Int
```

### Error Tests (eval/errors/ and invalid/)

**All error tests MUST include the error code `[EXXX]`** to ensure error code stability:

```
[call $+ 1 2 3]
=== out
[E020] arity mismatch
```

The `ERROR:` prefix (legacy format from before labeled sections) is still supported in
`=== out` for error corpus tests but is not required:

```
[call $error "boom"]
=== out
ERROR: [E024]
```

### Type Error Tests (eval/type_errors/)

```
[call $+ "hello" 42]
=== out
type mismatch
```

### Typecheck Warning Tests (typecheck/warnings/)

These tests deliberately trigger type checker warnings:

```
[f: [fn@String [x@Int] x]]
=== out
{"f": <function>}
=== warn
cannot unify Int with String
```

### Directives

Test files may include directives on the first line:

- `# no_fs` — Evaluate with filesystem access disabled

Directives MUST be on the first line and start with `#`. The directive line is stripped
before evaluation.

```
# no_fs
[include "file.llt"]
=== out
[E042] filesystem access is disabled
```

### No Sections

If no `=== ` section is present, the test only checks that:
- For `valid/` tests: the input parses without error
- For `invalid/` tests: the input fails to parse
- For `eval/` tests: this is an error (at least `=== out` is required)

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
- **type_system/**: Row polymorphism and type system evaluation tests
- **typeassert/**: TypeAssert edge cases and desugaring
- **typecheck/**: Type inference, polymorphism, let-generalization
- **functions/**: Function definitions, closures, variadic, named args
- **documents/**: Multi-document evaluation, pipelines with %
- **access/**: Access chain evaluation
- **underscore/**: _ implicit lambda desugaring
- **letrec/**: Letrec scoping and forward references

## Error Code Coverage

The corpus tests cover most error codes (E001-E099). Some error codes are difficult or
impossible to test in the corpus test framework:

### Not Corpus-Testable (Require Unit Tests)

- **E050 (IncludeNotAvailable):** Error code defined but never triggered in current codebase
- **E051 (IncludeIoError):** Requires actual filesystem and nonexistent files (tested in unit tests)
- **E052 (IncludeCycle):** Requires self-referencing files (tested in unit tests)
- **E053 (IncludeParseFailed):** Requires include with invalid syntax (tested in unit tests)
- **E054 (IncludeFileTooLarge):** Requires files >10MB (tested in unit tests)
- **E055 (IncludeHashMismatch):** Requires integrity hash validation (tested in unit tests)
- **E056 (IncludeHashRequired):** Requires `--require-integrity` flag (tested in unit tests)
- **E057 (IncludePathNotAllowed):** Requires `--allowed-paths` restriction (tested in unit tests)
- **E062 (JsonRange):** Unreachable with serde_json (defensive error, tested in unit tests)
- **E043 (ResourceLimitExceeded - collect):** Requires >1M elements (tested in unit tests)

All include-related errors (E050-E057) are comprehensively tested in `src/builtins.rs`
unit tests with temporary directories and cap-std sandboxing.

### Corpus-Testable Error Codes

All other error codes (E001-E099) have corpus tests in `tests/corpus/eval/errors/` or
`tests/corpus/invalid/`. Run `just test-corpus` to verify.
