# E055/E056 Hash Validation Tests

## E055: IncludeHashMismatch

Error raised when `[load source hash: "blake3:..."]` is called with a hash that doesn't match the actual content.

**Reachable:** Yes, in `builtin_load` at `builtins_meta.rs:1465`.

**Testing:** This requires:
1. A source string with known content
2. An incorrect `hash:` argument
3. Verification that E055 is raised with both expected and actual hashes in the message

**Why not a corpus test:** Corpus tests don't have easy access to the `load` builtin with controlled hash values. This is better tested via:
- Unit tests in `builtins_meta.rs` that call `builtin_load` directly
- Integration tests that use `--require-integrity` flag

## E056: IncludeHashRequired

Error raised when `--require-integrity` flag is set and `[load source]` is called without a `hash:` argument.

**Reachable:** Yes, in `builtin_load` at `builtins_meta.rs:1476`.

**Testing:** This requires:
1. Running with `--require-integrity` flag
2. Calling `load` without a `hash:` argument
3. Verification that E056 is raised

**Why not a corpus test:** Corpus tests don't control CLI flags. This is better tested via CLI integration tests that set `--require-integrity` and verify the error.

## Implementation Status

Both error constructors exist and are unit-tested in `error.rs`:
- `test_eval_error_include_hash_mismatch_constructor` (line 4458)
- `test_eval_error_include_hash_required_constructor` (line 4474)

The hash validation logic is exercised by the existing `load` builtin tests.
