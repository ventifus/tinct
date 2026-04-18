# Lazy Lisp Transformer

## Project Overview

A **unified data representation and transformation language** combining JSON-like data structures with lazy functional transformations.

**Vision:** One language for both data representation (like JSON/YAML) and data transformation (like JSONnet/jq), with lazy evaluation and strong composition principles.

**Current State:** Phase 0 (parser) complete. Phase 1a-1e (evaluator) complete. Phase 2a (core types & inference) complete. Phase 2b (polymorphism) complete. Phase 3a (Rust-native builtins) + 3a-llt (stdlib loading) complete. Phase 3b (CLI + JSON output) complete. Phase 3c ($include) complete. Phase 3d (error reporting polish) complete. Hand-written parser (E2) deferred to Phase 6.

## Key Documents

- [DESIGN.md](DESIGN.md) — Language design: vision, 61 confirmed decisions, open questions, implementation roadmap
- [SPEC.md](SPEC.md) — Formal parser specification: lexical/syntactic grammar (PEG), AST node types, static constraints, desugaring rules, examples

DESIGN.md is the source of truth for *what the language does*. SPEC.md is the source of truth for *how the parser works*.

## Architecture

The parser is built on [pest](https://pest.rs/) (PEG-based grammar in a separate `.pest` file). It produces a fully spanned AST for error reporting. The evaluator implements lazy evaluation with letrec dict scoping.

### File Structure

| File | Purpose |
|------|---------|
| `src/grammar.pest` | PEG grammar (lexical + syntactic rules) |
| `src/ast.rs` | AST types: `File`, `Document`, `Expr` (including `Rest` for `...`/`...name`), `Entry`, `Param`, `Annotation`, `Spanned<T>` |
| `src/parser.rs` | pest pairs to AST conversion + unit tests |
| `src/eval.rs` | Evaluator: `eval()`, `materialize()` (call-site span attachment, stack frame propagation), dict construction with letrec semantics, document evaluation with scope chains and `$$` pipeline, function evaluation (`fn`/`call`), fully lazy argument evaluation (call args wrapped as unevaluated thunks, builtin calls deferred via `PendingBuiltin`), `$_` implicit lambda desugaring, named args, variadics, arity checking, TypeAssert `default:` fallback, depth limit (256) |
| `src/builtins.rs` | 28 Rust-native builtins (arithmetic, comparison, control, dict, string, numeric, parsing, eval control, type introspection, I/O), `IncludeContext` + thread-local for `$include`, `standard_builtins()` registry, `create_root_env()`, `create_stdlib_env()` (loads `stdlib/prelude.llt`) |
| `src/value.rs` | Runtime types: `Value`, `Thunk` (lazy memoization with `Unevaluated`, `PendingBuiltin`, `InProgress`, `Materialized` states; `origin` label for stack traces), `Environment` (lexical scope chain), `BuiltinFn` signature (receives call-site `Span`) |
| `src/error.rs` | `EvalError` with definition-site span, materialization-site span, stack frames |
| `src/types.rs` | Type system: `Type` enum (Int, IntLiteral, Float, Str, StringLiteral, Bool, Number, Record, Function, TypeVar, Any), `RowRest` (Closed, Open, RowVar), `Substitution` (type variable bindings with `apply()`/`unify()`), `instantiate()` (fresh type variables per call site), `TypeEnv` (Rc-based scope chain with alias registry), `TypeError` |
| `src/typecheck.rs` | Type checker: `typecheck_file()`, `infer_expr()`, four-pass dict inference, access chain checking, TypeAssert enforcement, type alias expansion, polymorphic `check_call` (instantiate + unify + apply), `Fn@Return [Params]` resolution, row polymorphism (open/closed/row-var records) |
| `src/test_util.rs` | Shared test helpers: `test_span()`, `sp()` (test-only, `#[cfg(test)]`) |
| `src/lib.rs` | Public API: `parse()`, `parse_expression()`, `eval_source()`, `eval_file()`, `eval_file_with_input()`, `materialize()`, `deep_materialize()`, `create_stdlib_env()`, `set_include_context()`, `IncludeContext`, `json_to_value()`, `value_to_json()`, `value_to_display_string()` |
| `src/main.rs` | CLI (`llt` binary): `llt eval [OPTIONS] <FILE>` — evaluate LLT files, output JSON or LLT format, stdin JSON injection, `--eval` deep-forcing, `$include` context setup |
| `stdlib/prelude.llt` | LLT standard library: all stdlib functions implementable in LLT itself |
| `test_input.txt` | Example input demonstrating syntax |

### Dependencies

- `clap 4` (derive) — CLI argument parsing
- `pest 2.8` / `pest_derive 2.8` — PEG parser generator
- `indexmap 2.7` — insertion-ordered maps for dict entries
- `serde_json 1` — JSON parsing/serialization for `from-json` builtin and CLI output

## Testing

### Unit Tests
871 tests across `parser.rs`, `ast.rs`, `value.rs`, `error.rs`, `eval.rs`, `builtins.rs`, `types.rs`, `typecheck.rs`, `lib.rs`, and shared helpers in `test_util.rs`. Coverage includes every AST node type, Display/Debug formatting, access chains, special forms, annotations, document structure, static constraints, error cases, evaluator foundation types, core evaluation (literals, VarRef, dict letrec, cycle detection), access chain evaluation (dot, bracket, range, type assert, annotated), document evaluation (scope chains, `$$` pipeline, laziness, isolation), function evaluation (`fn` creates closures, `call` with arity checking, named args with defaults, variadics, builtin calls, `$_` implicit lambda desugaring, TypeAlias), eval depth limiting, materialization span propagation, error path coverage (non-dict access, string key ranges, invalid key types), all 28 Rust-native builtins (arithmetic auto-promotion, division by zero, comparison cross-type, `if` selective materialization, dict operations, string operations, numeric floor/round with NaN/infinity guards, string parsing, eval/error/try/apply, type-of, from-json, include with cycle detection/path resolution/nested includes/stdlib access), stdlib env loading (root env + prelude), type inference (literals, records, access chains, functions, scope chains, `$$` pipeline), subtyping (Number, structural records, function variance, open/closed/row-var records), TypeAssert enforcement, type alias resolution, annotation interpretation, `Fn@Return [Params]` function type expressions (one-param, two-param, concrete types, higher-order, error cases), row polymorphism (`...` rest entries, open record access, closed record rejection), type variable unification (Hindley-Milner, instantiation, substitution application, literal promotions), polymorphic function call checking (identity, multi-type-var, return-only type vars, arity mismatch fallthrough), and end-to-end pipeline integration (eval_file_with_input, JSON output, stdin JSON injection, display format, deep materialization).

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
  eval/
    builtins/       — builtin function evaluation
    errors/         — expected eval failures
    stdlib/         — stdlib function evaluation
```

Add a test: create a `.txt` file in the appropriate directory, then `just test-corpus`.

## Building and Running

All commands use containers (podman) — no local Rust installation required.

```bash
just build          # Build (debug)
just test           # Run all tests (unit + corpus)
just test-corpus    # Run only corpus tests
just run            # Eval test_input.txt, output JSON
just run-file FILE  # Eval a specific file
just run-llt FILE   # Eval with LLT display format
just run-json JSON FILE  # Eval with piped JSON stdin
just check          # Fast compile check
just fmt            # Format code
just shell          # Interactive container shell
just clean          # Clean build artifacts
```

Container: Rust 1.85, named volumes for `target/` and cargo cache, host UID/GID.

## Parser Design Notes

Key implementation details for anyone modifying the grammar or parser:

- **Whitespace-sensitive access chains**: `$a.b` is dot access, `$a .b` is two tokens. Achieved via pest compound-atomic (`${}`) rules on `access_expr`.
- **`annotation_value` must be non-atomic (`!{}`)**: Breaks compound-atomic inheritance from parent `param_annotation`, re-enabling whitespace inside annotation bracket expressions like `[type: Number default: 30]`.
- **Special form keywords**: Recognized by PEG ordered choice before falling back to `dict_entries`. Keywords are rejected if followed by `:` (so `call: x` is a dict entry, not a call form).
- **`var_ident` and `bare_word_char` use denylist**: Any character except structural delimiters (whitespace, `[]`, `:`, `;`, `#`, `"`, `@`) is valid. `var_ident` also excludes `.` (dot access). `bare_word_char` also excludes `$` (var_ref sigil). This means `$$` is VarRef("$"), `$$foo` is VarRef("$foo"), `$0` is valid, etc.
- **Document structure**: `file > document > expression`. `---` separates documents (total isolation, `$$` carries output). Sequential expressions within a document form a scope chain.
- **`doc_separator` uses `!bare_word_char` lookahead**: Prevents `----` from matching as a separator. The `!doc_separator` lookahead in `expression` stops documents from consuming `---`.
- **Pest stack overflow on deep nesting**: Pest recurses on Rust's call stack for nested brackets (`value -> bracket_expr -> dict_entries -> entry -> value`). The app-level `MAX_DEPTH` (256) check fires during AST construction, not during pest's parse phase. Inputs with ~500+ nested brackets can overflow the 8MB default stack before any app check fires. Accepted limitation of pest; resolved by Phase 6 (hand-written iterative parser).
