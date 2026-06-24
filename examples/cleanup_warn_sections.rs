//! Clean up stale `=== warn` sections in corpus test files.
//!
//! After the typecheck-import-env sprint wired `imports::build_prelude_env()` into
//! `typecheck_source()`, the type checker now knows about prelude functions.
//! Previously, tests had "undefined variable" warnings for prelude functions like
//! map, filter, and, or, flatten, zip, etc. Now those are resolved.
//!
//! This script:
//! 1. Finds all `*.llt-eval` files with `=== warn` sections
//! 2. For each file, runs the typecheck
//! 3. If typecheck now returns Ok (no warnings), removes the `=== warn` section
//! 4. If typecheck still has warnings, updates the `=== warn` section to match new output

use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

fn main() {
    let corpus_dir = PathBuf::from("tests/corpus");

    let mut files_processed = 0;
    let mut files_cleaned = 0;
    let mut files_updated = 0;

    for entry in WalkDir::new(&corpus_dir) {
        let entry = entry.unwrap();
        let path = entry.path();

        // Skip if not a .llt-eval file
        if !path.extension().map_or(false, |ext| ext == "llt-eval") {
            continue;
        }

        // Read file content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error reading {}: {}", path.display(), e);
                continue;
            }
        };

        // Skip if no === warn section
        if !content.contains("=== warn") {
            continue;
        }

        files_processed += 1;

        // Split into parts
        let parts: Vec<&str> = content.split("===").collect();
        if parts.len() < 2 {
            eprintln!("Malformed test file: {}", path.display());
            continue;
        }

        // Extract input (before first ===), stripping the directive line
        // if the first line starts with '#' (matches split_test_file in corpus_tests.rs).
        let raw_input = parts[0];
        let input = if let Some(newline_pos) = raw_input.find('\n') {
            let first_line = raw_input[..newline_pos].trim();
            if first_line.starts_with('#') {
                &raw_input[newline_pos + 1..]
            } else {
                raw_input
            }
        } else {
            raw_input
        };
        let input = input.trim();

        // Run typecheck (this handles parsing internally)
        let typecheck_result = tinct::async_rt::block_on_anywhere(tinct::typecheck_source(input));

        if typecheck_result.is_ok() {
            // No warnings anymore — remove === warn section
            let new_content = remove_warn_section(&content);

            match fs::write(path, new_content) {
                Ok(_) => {
                    files_cleaned += 1;
                    println!("Cleaned: {}", path.display());
                }
                Err(e) => {
                    eprintln!("Error writing {}: {}", path.display(), e);
                }
            }
        } else {
            // Still has warnings — update === warn section with current output
            let new_warnings = typecheck_result.unwrap_err();
            let new_content = update_warn_section(&content, &new_warnings);

            // Only write if content changed
            if new_content != content {
                match fs::write(path, new_content) {
                    Ok(_) => {
                        files_updated += 1;
                        println!("Updated: {}", path.display());
                    }
                    Err(e) => {
                        eprintln!("Error writing {}: {}", path.display(), e);
                    }
                }
            }
        }
    }

    println!("\nSummary:");
    println!("  Files processed: {}", files_processed);
    println!("  Files cleaned (=== warn removed): {}", files_cleaned);
    println!("  Files updated (=== warn modified): {}", files_updated);
}

/// Remove the === warn section from a test file.
fn remove_warn_section(content: &str) -> String {
    let parts: Vec<&str> = content.split("=== warn").collect();
    if parts.len() != 2 {
        // Unexpected format — return unchanged
        return content.to_string();
    }

    // parts[0] is everything before === warn
    // parts[1] is the warn content plus anything after

    // Check if there's another === after warn (e.g., === error)
    let after_warn = parts[1];
    if let Some(next_section_pos) = after_warn.find("===") {
        // There's another section after warn — keep it
        format!("{}{}", parts[0].trim_end(), &after_warn[next_section_pos..])
    } else {
        // No other sections — just remove warn and trailing content
        parts[0].trim_end().to_string() + "\n"
    }
}

/// Update the === warn section with new warning text.
fn update_warn_section(content: &str, new_warnings: &str) -> String {
    let parts: Vec<&str> = content.split("=== warn").collect();
    if parts.len() != 2 {
        // Unexpected format — return unchanged
        return content.to_string();
    }

    let before_warn = parts[0];
    let after_warn = parts[1];

    // Check if there's another === after warn
    let after_warn_content = if let Some(next_section_pos) = after_warn.find("===") {
        &after_warn[next_section_pos..]
    } else {
        ""
    };

    format!(
        "{}=== warn\n{}\n{}",
        before_warn,
        new_warnings.trim(),
        after_warn_content
    )
}
