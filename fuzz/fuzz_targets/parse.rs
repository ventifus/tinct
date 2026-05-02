#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes through the parser.
// Goals:
//   - No panics or stack overflows regardless of input
//   - MAX_PARSE_DEPTH=256 must catch all deep nesting before Rust stack exhaustion
//   - Invalid UTF-8 must be silently rejected, not panic
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Both parse() and parse_expression() must not panic
        let _ = tinct::parse(s);
        let _ = tinct::parse_expression(s);
    }
});
