# Lazy Lisp Transformer

## Project Overview

A **unified data representation and transformation language** combining JSON-like data structures with lazy functional transformations.

**Vision:** One language for both data representation (like JSON/YAML) and data transformation (like JSONnet/jq), with lazy evaluation and strong composition principles.

**Current State:** Phase 0 (parser) complete. Evaluator and transformation engine are next.

## Key Documents

- [DESIGN.md](DESIGN.md) — Language design: vision, 61 confirmed decisions, open questions, implementation roadmap
- [SPEC.md](SPEC.md) — Formal parser specification: lexical/syntactic grammar (PEG), AST node types, static constraints, desugaring rules, examples

DESIGN.md is the source of truth for *what the language does*. SPEC.md is the source of truth for *how the parser works*.

## Architecture

The parser is built on [pest](https://pest.rs/) (PEG-based grammar in a separate `.pest` file). It produces a fully spanned AST for error reporting.

### File Structure

| File | Purpose |
|------|---------|
| `src/grammar.pest` | PEG grammar (lexical + syntactic rules) |
| `src/ast.rs` | AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>` |
| `src/parser.rs` | pest pairs to AST conversion + unit tests |
| `src/lib.rs` | Public API: `parse(input) -> Result<Spanned<File>, ParseError>`, `parse_expression` convenience |
| `src/main.rs` | CLI: read file (max 10MB), parse, print AST |
| `test_input.txt` | Example input demonstrating syntax |

### Dependencies

- `pest 2.8` / `pest_derive 2.8` — PEG parser generator

## Testing

### Unit Tests
191 tests across `parser.rs`, `ast.rs`, `value.rs`, and `error.rs` covering every AST node type, Display/Debug formatting, access chains, special forms, annotations, document structure, static constraints, error cases, and evaluator foundation types.

### Corpus Tests (`tests/corpus/`)
File-based test suite with auto-discovery. Each `.txt` file is parsed; valid inputs must succeed, invalid inputs must fail. Uses `===` as the delimiter between input and expected output (not `---`, which is a valid LLT document separator).

```
tests/corpus/
  valid/
    literals/       — int, float, bool, string, bare word, var ref
    special_forms/  — call, fn, type
    access/         — dot, bracket, chained, range, space-prevents-access
    annotations/    — type assert (simple + dict)
    documents/      — multi-expression, multi-document, --- separator
    complex/        — full config, pipeline, conditionals, comments, semicolons
    simple/         — basic key-value pairs, nesting
    edge_cases/     — empty input, whitespace
  invalid/
    syntax_errors/  — missing bracket, extra tokens, unexpected colon, missing value
```

Add a test: create a `.txt` file in the appropriate directory, then `just test-corpus`.

## Building and Running

All commands use containers (podman) — no local Rust installation required.

```bash
just build          # Build (debug)
just test           # Run all tests (unit + corpus)
just test-corpus    # Run only corpus tests
just run            # Parse test_input.txt
just check          # Fast compile check
just fmt            # Format code
just shell          # Interactive container shell
just clean          # Clean build artifacts
```

Container: Rust 1.83, named volumes for `target/` and cargo cache, host UID/GID.

## Parser Design Notes

Key implementation details for anyone modifying the grammar or parser:

- **Whitespace-sensitive access chains**: `$a.b` is dot access, `$a .b` is two tokens. Achieved via pest compound-atomic (`${}`) rules on `access_expr`.
- **`annotation_value` must be non-atomic (`!{}`)**: Breaks compound-atomic inheritance from parent `param_annotation`, re-enabling whitespace inside annotation bracket expressions like `[type: Number default: 30]`.
- **Special form keywords**: Recognized by PEG ordered choice before falling back to `dict_entries`. Keywords are rejected if followed by `:` (so `call: x` is a dict entry, not a call form).
- **`var_ident` and `bare_word_char` use denylist**: Any character except structural delimiters (whitespace, `[]`, `:`, `;`, `#`, `"`, `@`) is valid. `var_ident` also excludes `.` (dot access). `bare_word_char` also excludes `$` (var_ref sigil). This means `$$` is VarRef("$"), `$$foo` is VarRef("$foo"), `$0` is valid, etc.
- **Document structure**: `file > document > expression`. `---` separates documents (total isolation, `$$` carries output). Sequential expressions within a document form a scope chain.
- **`doc_separator` uses `!bare_word_char` lookahead**: Prevents `----` from matching as a separator. The `!doc_separator` lookahead in `expression` stops documents from consuming `---`.
