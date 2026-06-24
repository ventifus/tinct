#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes through the type checker (parse → desugar → typecheck).
// Goals:
//   - No panics regardless of type annotations or deeply-nested record types
//   - MAX_SUBST_SIZE=50_000 must cap O(N²) dot-access type inference DoS
//   - Open-record unification must not loop
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = tinct::async_rt::block_on_anywhere(tinct::typecheck_source(s));
    }
});
