//! Add `=== warn` sections to eval/errors/ corpus files that produce type warnings.
//!
//! This is a one-time migration for the corpus-section-consistency sprint.
//! Error files now check both eval errors and type warnings independently.

use std::fs;
use std::path::PathBuf;
use tinct::typecheck_source;

fn main() {
    let errors_dir = PathBuf::from("tests/corpus/eval/errors");

    let mut files_updated = 0;
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

        // Skip if already has === warn
        if content.contains("\n=== warn\n") {
            files_skipped += 1;
            continue;
        }

        // Extract source (before first ===)
        let source = if let Some(pos) = content.find("\n===") {
            // Strip directive line if present
            let first_line_end = content.find('\n').unwrap_or(content.len());
            let first_line = &content[..first_line_end];
            if first_line.trim().starts_with('#') {
                &content[first_line_end + 1..pos + 1]
            } else {
                &content[..pos + 1]
            }
        } else {
            eprintln!("Skipping {} (no === delimiter found)", path.display());
            continue;
        };

        // Check if typecheck produces warnings
        let warnings = match typecheck_source(source) {
            Ok(()) => {
                // No warnings
                files_skipped += 1;
                continue;
            }
            Err(w) => w,
        };

        // Insert === warn section after === error
        let new_content = if let Some(pos) = content.find("\n=== error\n") {
            let error_section_start = pos + 11; // length of "\n=== error\n"
                                                // Find the end of the error section (either EOF or next ===)
            let after_error = &content[error_section_start..];
            let error_section_end = if let Some(next_section) = after_error.find("\n===") {
                error_section_start + next_section
            } else {
                content.len()
            };

            // Build new content: before error section + error section + warn section + rest
            let mut new_content = String::new();
            new_content.push_str(&content[..error_section_end]);
            new_content.push_str("\n=== warn\n");
            new_content.push_str(&warnings);
            new_content.push('\n');
            if error_section_end < content.len() {
                new_content.push_str(&content[error_section_end..]);
            }
            new_content
        } else {
            eprintln!("Skipping {} (no === error section found)", path.display());
            continue;
        };

        // Write back
        if let Err(e) = fs::write(&path, new_content) {
            eprintln!("Error writing {}: {}", path.display(), e);
            continue;
        }

        files_updated += 1;
        println!("✓ {}", path.file_name().unwrap().to_string_lossy());
    }

    println!();
    println!("Summary:");
    println!("  {} files updated", files_updated);
    println!(
        "  {} files skipped (already had === warn or no warnings)",
        files_skipped
    );
}
