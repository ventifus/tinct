# E030 Source File Display

## Changed in error-source-file sprint

The `EvalError` struct now has a `source_file: Option<Arc<str>>` field that is populated when errors occur during file evaluation. The Display implementation shows the filename as a prefix to the span when present.

**Before:**
```
[E030] duplicate key: raise (defined at 2132:1-2132:66)
```

**After (when source_file is set):**
```
[E030] duplicate key: raise (defined at myfile.llt:2132:1-2132:66)
```

## Implementation

- `EvalError::duplicate_key` now accepts `source_file: Option<&str>` parameter
- `EvalError::undefined_variable` now accepts `source_file: Option<&str>` parameter
- Call sites in `eval_dict.rs` and `eval.rs` pass `ctx.config.source_file.as_deref()`
- All other constructors initialize `source_file: None`

## Testing

This feature is tested via:
1. CLI tests that include files and verify error messages contain filenames
2. Unit tests in `error.rs` verify constructors work with and without source_file
3. The existing include path tests exercise the full pipeline

Corpus tests cannot easily test this feature because they operate on single files, and the source_file is only populated during `eval_file` calls in the CLI, not during isolated `eval_source` calls used by the test harness.
