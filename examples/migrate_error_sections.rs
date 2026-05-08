//! Migrate `=== out` to `=== error` in eval/errors/ corpus test files.
//!
//! The corpus-section-consistency sprint unifies all corpus runners behind a single
//! shared engine. The contract is:
//! - `=== out` — eval succeeds; output matches substring.
//! - `=== error` — eval fails; message matches substring. Must include [EXXX] error code.
//!
//! Files in `tests/corpus/eval/errors/` currently use `=== out` to hold error substrings.
//! This script migrates them to `=== error`.

use std::fs;
use std::path::PathBuf;

fn main() {
    let errors_dir = PathBuf::from("tests/corpus/eval/errors");

    let mut files_migrated = 0;
    let mut files_skipped = 0;

    for entry in fs::read_dir(&errors_dir).expect("Failed to read errors directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();

        // Skip if not a .llt-eval file
        if !path.extension().map_or(false, |ext| ext == "llt-eval") {
            continue;
        }

        // Read file content
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error reading {}: {}", path.display(), e);
                continue;
            }
        };

        // Check if file has `=== out` delimiter
        if !content.contains("\n=== out\n") {
            files_skipped += 1;
            continue;
        }

        // Migrate === out to === error
        let new_content = content.replace("\n=== out\n", "\n=== error\n");

        // Write back
        if let Err(e) = fs::write(&path, new_content) {
            eprintln!("Error writing {}: {}", path.display(), e);
            continue;
        }

        files_migrated += 1;
        println!("✓ {}", path.file_name().unwrap().to_string_lossy());
    }

    println!();
    println!("Migration complete:");
    println!("  {} files migrated", files_migrated);
    println!("  {} files skipped (already migrated)", files_skipped);
}
