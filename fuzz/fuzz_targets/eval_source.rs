#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes through the full eval pipeline (parse → desugar → typecheck → eval).
// no_fs=true prevents filesystem access from fuzz inputs.
// Goals:
//   - No panics or infinite loops regardless of input
//   - MAX_EVAL_DEPTH=256 must catch infinite recursion before Rust stack exhaustion
//   - MAX_SUBST_SIZE=50_000 must bound type inference resource use
//   - Circular dependencies must return EvalError, not hang
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = tinct::async_rt::block_on_anywhere(tinct::eval_source_with_config(s, true /* no_fs */));
    }
});
