//! Add === warn sections to corpus files that produce type warnings.
//!
//! Usage: cargo run --example add_warn_sections
//!
//! This tool finds all .llt-eval files in tests/corpus/eval/ (excluding errors/ and type_errors/)
//! and tests/corpus/valid/, runs typecheck_source() on each, and adds a === warn section if
//! typecheck produces warnings and the file doesn't already have one.

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("Failed to get current directory"));

    // Find all test files
    let mut test_files = Vec::new();

    // tests/corpus/eval/ (exclude errors/ and type_errors/)
    let eval_dir = manifest_dir.join("tests/corpus/eval");
    let errors_dir = eval_dir.join("errors");
    let type_errors_dir = eval_dir.join("type_errors");

    for file in find_test_files(&eval_dir) {
        if !file.starts_with(&errors_dir) && !file.starts_with(&type_errors_dir) {
            test_files.push(file);
        }
    }

    // tests/corpus/valid/
    let valid_dir = manifest_dir.join("tests/corpus/valid");
    test_files.extend(find_test_files(&valid_dir));

    // Use a thread with large stack to prevent overflow during typecheck
    // (same rationale as corpus test runners)
    let manifest_dir_clone = manifest_dir.clone();
    let test_files_clone = test_files.clone();
    let total_files = test_files.len();

    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024) // 512MB
        .spawn(move || process_files(&test_files_clone, &manifest_dir_clone))
        .unwrap()
        .join()
        .unwrap();

    let (updated, has_warn, clean) = result;

    println!("\nSummary:");
    println!("  Updated: {}", updated);
    println!("  Already had === warn: {}", has_warn);
    println!("  Clean (no warnings): {}", clean);
    println!("  Total processed: {}", total_files);
}

fn process_files(test_files: &[PathBuf], manifest_dir: &Path) -> (usize, usize, usize) {
    let mut updated = 0;
    let mut has_warn = 0;
    let mut clean = 0;

    for test_file in test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        // Check if already has === warn section
        if content.contains("\n=== warn") {
            has_warn += 1;
            continue;
        }

        // Extract source (before first === section)
        let source = extract_source(&content);

        // Run typecheck
        match tinct::async_rt::block_on_anywhere(tinct::typecheck_source(&source)) {
            Ok(()) => {
                // Clean - no warnings
                clean += 1;
            }
            Err(warnings) => {
                // Has warnings - add === warn section
                let new_content = add_warn_section(&content, &warnings);
                fs::write(test_file, new_content)
                    .unwrap_or_else(|e| panic!("Failed to write {}: {}", test_file.display(), e));

                let relative_path = test_file.strip_prefix(manifest_dir).unwrap_or(test_file);
                println!("Added === warn to {}", relative_path.display());
                updated += 1;
            }
        }
    }

    (updated, has_warn, clean)
}

fn find_test_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let entry = entry.expect("failed to read directory entry");
            let path = entry.path();
            if path.is_dir() {
                files.extend(find_test_files(&path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("llt-eval") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn extract_source(content: &str) -> String {
    // Strip directive line if present
    let content = if let Some(newline_pos) = content.find('\n') {
        let (first_line, remainder) = content.split_at(newline_pos);
        if first_line.trim().starts_with('#') {
            &remainder[1..] // skip the newline
        } else {
            content
        }
    } else {
        content
    };

    // Find first === section
    if let Some(pos) = content.find("\n===") {
        content[..pos + 1].to_string() // include trailing newline before ===
    } else {
        content.to_string()
    }
}

fn add_warn_section(content: &str, warnings: &str) -> String {
    // Find the end of the last existing section
    // We want to add === warn after all existing sections

    let mut new_content = content.to_string();

    // Ensure content ends with newline before adding new section
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    new_content.push_str("=== warn\n");
    new_content.push_str(warnings);

    // Ensure final newline
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    new_content
}
