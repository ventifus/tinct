# Async Builtin Transformation Plan

## Sprint: sprint-2b-builtins-async

This document tracks the async transformation of all builtin functions.

## Status

### Completed Files
- ✅ `src/value.rs` — BuiltinFn type changed to return `Pin<Box<dyn Future<...>>>`  
- ✅ `src/eval_materialize.rs` — All `(def.func)(builtin_args)` calls now have `.await` (2 locations)
- ✅ `src/builtins_dict.rs` — All 9 builtins wrapped in `Box::pin(async move { ... })`
- ✅ `src/builtins_uri.rs` — All 3 builtins wrapped in `Box::pin(async move { ... })`

### Remaining Files (159 builtins)
These files need the same transformation pattern applied:

1. **Add imports** at top of file (after existing `use std::` lines):
   ```rust
   use std::future::Future;
   use std::pin::Pin;
   ```

2. **Transform each builtin function**:
   - Change signature: `-> EvalResult<Arc<Thunk>>` → `-> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>>`
   - Wrap body in `Box::pin(async move { ... })`
   - If the builtin calls `materialize()`, change to `materialize(...).await` (it's currently aliased as `materialize_sync`)
   - If the builtin calls `invoke_function()`, change to `invoke_function(...).await` (it's currently aliased as `invoke_function_sync`)

### Files and Builtin Counts
- ✅ `src/builtins_dict.rs` — 9 builtins (DONE)
- ✅ `src/builtins_uri.rs` — 3 builtins (DONE)
- ⬜ `src/builtins.rs` — 11 builtins
- ⬜ `src/builtins_bytes.rs` — 5 builtins
- ⬜ `src/builtins_datetime.rs` — (count: check with grep)
- ⬜ `src/builtins_io.rs` — 38 builtins
- ⬜ `src/builtins_math.rs` — 29 builtins
- ⬜ `src/builtins_meta.rs` — 35 builtins (includes `builtin_apply_impl` which needs special handling)
- ⬜ `src/builtins_seq_gen.rs` — 7 builtins
- ⬜ `src/builtins_seq_prim.rs` — 5 builtins
- ⬜ `src/builtins_seq_reduce.rs` — 4 builtins
- ⬜ `src/builtins_seq_xform.rs` — 7 builtins
- ⬜ `src/builtins_string.rs` — 18 builtins

## Special Cases

### builtins_meta.rs
- `builtin_apply_impl` calls `(def.func)(builtin_args)` at line 410
- This is already inside a `Box::pin(async move { ... })` context once `builtin_apply` is transformed
- Change line 410 to: `(def.func)(builtin_args).await`
- Also has calls to `materialize()` and `invoke_function()` that need `.await`

### materialize() calls
All builtins currently import `use crate::eval::materialize_sync as materialize`.
After wrapping in `async move {}`, change:
- `materialize(&thunk, span, &ctx)?` → `materialize(&thunk, span, &ctx).await?`

Lines with `// H2:` or `// H3:` comments mark these call sites.

### invoke_function() calls
All builtins currently import `use crate::eval_call::invoke_function_sync as invoke_function`.
After wrapping in `async move {}`, change:
- `invoke_function(&call_ctx)?` → `invoke_function(&call_ctx).await?`

## Transformation Script

```bash
#!/bin/bash
# Transform all builtin files to async

FILES=(
    "src/builtins.rs"
    "src/builtins_bytes.rs"
    "src/builtins_datetime.rs"
    "src/builtins_io.rs"
    "src/builtins_math.rs"
    "src/builtins_meta.rs"
    "src/builtins_seq_gen.rs"
    "src/builtins_seq_prim.rs"
    "src/builtins_seq_reduce.rs"
    "src/builtins_seq_xform.rs"
    "src/builtins_string.rs"
)

for file in "${FILES[@]}"; do
    echo "Transforming $file..."
    
    # 1. Add imports (if not present)
    if ! grep -q "use std::future::Future" "$file"; then
        # Find first "use std::" line and add imports after it
        sed -i '/^use std::/a\
use std::future::Future;\
use std::pin::Pin;' "$file"
    fi
    
    # 2. Transform function signatures
    # This requires careful regex - match function signature and add async wrapper
    # Pattern: pub(crate) fn builtin_NAME(...) -> EvalResult<Arc<Thunk>> {
    # Replace with opening async wrapper (closing brace must be added manually at end of function)
    
    perl -i -pe 's/(pub\(crate\)\s+fn\s+builtin_\w+\([^)]+\))\s*->\s*EvalResult<Arc<Thunk>>\s*\{/$1 -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {\n    Box::pin(async move {/g' "$file"
    
    # 3. Change materialize() to materialize().await
    # Only inside async blocks (after Box::pin(async move {)
    sed -i 's/materialize(\([^)]*\))?/materialize(\1).await?/g' "$file"
    
    # 4. Change invoke_function() to invoke_function().await
    sed -i 's/invoke_function(\([^)]*\))?/invoke_function(\1).await?/g' "$file"
    
    echo "  ✓ Basic transformations done for $file"
    echo "  ⚠️  MANUAL: Add closing }) before each function's closing brace"
done

echo ""
echo "⚠️  IMPORTANT: The script added Box::pin(async move { but you must manually add"
echo "   the closing }) before each function's final closing brace!"
echo ""
echo "   For each builtin function, find its closing brace and add }) before it:"
echo "   Old:  ...final_statement"
echo "         }"
echo "   New:  ...final_statement"
echo "         })"
echo "   }"
```

## Manual Steps Required

The script above handles most of the transformation, but **closing braces must be added manually** for each function.

For each builtin function in the transformed files:
1. Find the function's final closing brace `}`
2. Add `})` on the line before it (to close the `Box::pin(async move { ... })`)

Example:
```rust
// Before
pub(crate) fn builtin_foo(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    // ... function body ...
    ok_val(result, call_span)
}

// After step 1 (script)
pub(crate) fn builtin_foo(ctx_arg: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        // ... function body ...
        ok_val(result, call_span)
}  // ← WRONG! Missing closing })

// After step 2 (manual)
pub(crate) fn builtin_foo(ctx_arg: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        // ... function body ...
        ok_val(result, call_span)
    })  // ← CORRECT! Closes Box::pin(async move { ... })
}
```

## Verification

After transformation:
1. Run `just build` — should compile without errors
2. Check that all `(def.func)(...)` call sites have `.await`
3. Check that all `materialize(...)` calls in builtins have `.await`
4. Check that all `invoke_function(...)` calls in builtins have `.await`

## Additional Changes After Builtin Transformation

Once all builtins are transformed:

### Step 4: Delete sync wrappers
- Delete `materialize_sync()` from `src/eval.rs`
- Delete `invoke_function_sync()` from `src/eval_call.rs`
- Update remaining non-builtin callers (expand.rs, type_normalize.rs, formatter.rs) to use `block_on_anywhere(materialize(...))` directly

### Step 5: Fix RLIMIT_AS debug OOM
In `src/main.rs`, wrap the RLIMIT_AS setrlimit call with:
```rust
#[cfg(not(debug_assertions))]
{
    // RLIMIT_AS code here
}
```

### Step 6: Make main() async
- Add `#[tokio::main]` attribute to `fn main()` in `src/main.rs`
- Change signature to `async fn main()`
- Replace all `block_on_anywhere(...)` calls in main with direct `.await`

### Step 7: Update tests
- Change `#[test]` to `#[tokio::test]` in test modules that call async eval functions
- Keep existing sync shadow functions in test modules (they use block_on_anywhere)

## Notes

- This is a massive but mechanical transformation
- Pre-1.0 status means refactor freely
- The CPS transformation (sprint-2b-builtins-cps) already removed all direct `materialize()` calls from builtin bodies, making this transformation safe
- After this sprint, the entire eval pipeline will be fully async
