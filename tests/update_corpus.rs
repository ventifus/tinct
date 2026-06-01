// Corpus test updater — rewrites expected output sections in .llt-eval files
// to match actual evaluator / typechecker output.
//
// Usage:
//   cargo test --test update_corpus -- --ignored [--nocapture]
//
// This runs as an ignored test so it never executes during normal `cargo test`.
// The single test function walks every .llt-eval file under tests/corpus/eval/,
// evaluates each one through the same pipeline as the real corpus runner, and
// rewrites the expected sections (=== out, === error, === warn) to match the
// actual output.
//
// Environment variables:
//   UPDATE_CORPUS_DRY_RUN=1   — print diffs without modifying files
//   UPDATE_CORPUS_FILTER=pat  — only process files whose path contains `pat`

#![allow(
    clippy::disallowed_methods,
    clippy::useless_format,
    clippy::manual_ok_err
)]

mod test_helpers;

use std::fs;
use std::path::PathBuf;
use test_helpers::{find_test_files, split_test_file};

/// Check if error message contains an error code pattern like [E001], [E099], [T000], etc.
/// Copied from test_helpers.rs.
fn has_error_code_prefix(error_msg: &str) -> bool {
    error_msg.chars().collect::<Vec<_>>().windows(6).any(|w| {
        w[0] == '['
            && (w[1] == 'E' || w[1] == 'T')
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[4].is_ascii_digit()
            && w[5] == ']'
    })
}

/// Evaluate a test file through the same pipeline as the eval corpus runner.
/// Returns (output, error, warnings).
fn eval_test(
    input: &str,
    no_fs: bool,
    cap_net: &[(String, String)],
) -> (Option<String>, Option<String>, Option<String>) {
    // Clear stdlib cache to prevent memory accumulation (same as corpus runner).
    tinct::clear_stdlib_cache();

    let eval_result = if cap_net.is_empty() {
        tinct::eval_source_with_config(input, no_fs)
    } else {
        tinct::eval_source_with_cap_net(input, no_fs, cap_net)
    };

    let (output, error) = match eval_result {
        Ok(actual) => (Some(actual), None),
        Err(e) => (None, Some(format!("{e}"))),
    };

    let warnings = match tinct::typecheck_source_errors_only(input) {
        Ok(()) => None,
        Err(type_errors) => Some(type_errors),
    };

    (output, error, warnings)
}

/// Reconstruct a test file from its parts.
fn rebuild_test_file(
    directive_line: Option<&str>,
    source: &str,
    output: Option<&str>,
    error: Option<&str>,
    warnings: Option<&str>,
) -> String {
    let mut result = String::new();

    // Directive line (# no_fs, # cap_net, etc.)
    if let Some(dir) = directive_line {
        result.push_str(dir);
        result.push('\n');
    }

    // Source code — ensure it ends with a newline before sections
    result.push_str(source);
    if !source.ends_with('\n') {
        result.push('\n');
    }

    // Output section (=== out or === error, but not both)
    if let Some(out) = output {
        result.push_str("=== out\n");
        result.push_str(out);
        if !out.ends_with('\n') {
            result.push('\n');
        }
    } else if let Some(err) = error {
        result.push_str("=== error\n");
        result.push_str(err);
        if !err.ends_with('\n') {
            result.push('\n');
        }
    }

    // Warning section
    if let Some(warn) = warnings {
        result.push_str("\n=== warn\n");
        result.push_str(warn);
        if !warn.ends_with('\n') {
            result.push('\n');
        }
    }

    result
}

/// Extract the directive/comment line and source from raw file content.
/// Returns (first_hash_line, source_without_first_line).
///
/// Mirrors `split_test_file` behavior: if line 1 starts with `#`, it is
/// ALWAYS stripped from the source, whether it's a real directive or just
/// a comment. This matches the documented contract:
///   "If the first line starts with `#`, it is treated as a directive line
///    and is STRIPPED from the input before evaluation."
fn extract_parts(content: &str) -> (Option<String>, String) {
    if let Some(newline_pos) = content.find('\n') {
        let first_line = &content[..newline_pos];
        if first_line.trim().starts_with('#') {
            // Line 1 starts with # — always strip it from source
            let trimmed = first_line.trim();
            let rest = &content[newline_pos + 1..];
            let source = extract_source(rest);
            return (Some(trimmed.to_string()), source);
        }
    }

    // No # line — source starts at beginning
    let source = extract_source(content);
    (None, source)
}

/// Extract source code (everything before the first `\n===` delimiter).
fn extract_source(content: &str) -> String {
    if let Some(pos) = content.find("\n===") {
        // Include the content up to (but not including) the \n=== delimiter
        // The source should include the trailing newline before ===
        content[..pos + 1].to_string()
    } else {
        // No delimiter — entire content is source
        content.to_string()
    }
}

#[test]
#[ignore] // Only run explicitly: cargo test --test update_corpus -- --ignored
fn update_eval_corpus() {
    let dry_run = std::env::var("UPDATE_CORPUS_DRY_RUN").is_ok_and(|v| v == "1" || v == "true");
    let filter = std::env::var("UPDATE_CORPUS_FILTER").ok();

    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval");
    // Exclude type_errors/ — those use typecheck_source (not errors_only) and a different assertion model
    let type_errors_dir = corpus_dir.join("type_errors");

    let all_files = find_test_files(&corpus_dir);
    let test_files: Vec<_> = all_files
        .into_iter()
        .filter(|p| !p.starts_with(&type_errors_dir))
        .filter(|p| {
            if let Some(ref pat) = filter {
                p.to_string_lossy().contains(pat.as_str())
            } else {
                true
            }
        })
        .collect();

    eprintln!(
        "update_eval_corpus: {} files to process (dry_run={})",
        test_files.len(),
        dry_run
    );

    let mut updated = 0;
    let mut unchanged = 0;
    let mut errors = 0;
    let mut skipped = 0;

    for (i, test_file) in test_files.iter().enumerate() {
        let content = match fs::read_to_string(test_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ERROR reading {}: {}", test_file.display(), e);
                errors += 1;
                continue;
            }
        };

        let relative = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        // Parse the existing test file to get directives and input
        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ERROR parsing {}: {}", relative.display(), e);
                errors += 1;
                continue;
            }
        };

        // Extract directive line and source from raw content
        let (directive_line, source) = extract_parts(&content);

        // Run through the eval pipeline
        let (output, error, warnings) = eval_test(&test.input, test.no_fs, &test.cap_net);

        // Guard: if the error lacks an [EXXX] code, skip this file.
        // The corpus runner requires all === error sections to have an error code.
        // Errors without codes are typically parse errors from broken test source
        // that needs manual investigation.
        if let Some(ref err) = error {
            if !has_error_code_prefix(err) {
                eprintln!(
                    "  SKIP {}: error lacks [EXXX] code: {}",
                    relative.display(),
                    truncate(err, 100)
                );
                skipped += 1;
                continue;
            }
        }

        // Regression guard: if the existing file expected success (=== out) but the
        // new result is an error, do NOT overwrite it. Silently accepting a regression
        // as "expected behavior" would mask real bugs in CI.
        if test.expectations.out.is_some() && error.is_some() {
            eprintln!(
                "  SKIP: previously-passing test now produces error — possible regression: {}",
                relative.display()
            );
            skipped += 1;
            continue;
        }

        // Rebuild the file
        let new_content = rebuild_test_file(
            directive_line.as_deref(),
            &source,
            output.as_deref().map(|s| s.trim()),
            error.as_deref(),
            warnings.as_deref(),
        );

        if new_content == content {
            unchanged += 1;
        } else {
            updated += 1;
            if dry_run {
                eprintln!(
                    "  [{}/{}] WOULD UPDATE: {}",
                    i + 1,
                    test_files.len(),
                    relative.display()
                );
                // Show a brief diff summary
                let old_has_out = test.expectations.out.is_some();
                let old_has_err = test.expectations.error.is_some();
                let old_has_warn = test.expectations.warn.is_some();
                let new_has_out = output.is_some();
                let new_has_err = error.is_some();
                let new_has_warn = warnings.is_some();
                if old_has_out != new_has_out || old_has_err != new_has_err {
                    eprintln!(
                        "    outcome: out={}->{} error={}->{} warn={}->{}",
                        old_has_out,
                        new_has_out,
                        old_has_err,
                        new_has_err,
                        old_has_warn,
                        new_has_warn
                    );
                }
                if let Some(ref old_out) = test.expectations.out {
                    if let Some(ref new_out) = output {
                        if old_out.trim() != new_out.trim() {
                            eprintln!("    out changed:");
                            eprintln!("      old: {}", truncate(old_out, 120));
                            eprintln!("      new: {}", truncate(new_out.trim(), 120));
                        }
                    }
                }
                if let Some(ref old_err) = test.expectations.error {
                    if let Some(ref new_err) = error {
                        if !new_err.contains(old_err.as_str()) {
                            eprintln!("    error changed:");
                            eprintln!("      old: {}", truncate(old_err, 120));
                            eprintln!("      new: {}", truncate(new_err, 120));
                        }
                    }
                }
            } else {
                if (i + 1) % 50 == 0 || i == 0 {
                    eprintln!(
                        "  [{}/{}] updating: {}",
                        i + 1,
                        test_files.len(),
                        relative.display()
                    );
                }
                fs::write(test_file, &new_content).unwrap_or_else(|e| {
                    eprintln!("  ERROR writing {}: {}", test_file.display(), e);
                });
            }
        }

        // Periodic memory cleanup
        if (i + 1) % 100 == 0 {
            tinct::clear_stdlib_cache();
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }

    eprintln!("\n=== Summary ===");
    eprintln!("  Total:     {}", test_files.len());
    eprintln!("  Updated:   {}", updated);
    eprintln!("  Unchanged: {}", unchanged);
    eprintln!("  Skipped:   {}", skipped);
    eprintln!("  Errors:    {}", errors);

    if dry_run {
        eprintln!("\n  (dry run — no files modified)");
    }
}

fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() > max {
        format!("{}...", &first_line[..max])
    } else {
        first_line.to_string()
    }
}

/// Update the valid corpus (parse-only tests).
#[test]
#[ignore]
fn update_valid_corpus() {
    let dry_run = std::env::var("UPDATE_CORPUS_DRY_RUN").is_ok_and(|v| v == "1" || v == "true");
    let filter = std::env::var("UPDATE_CORPUS_FILTER").ok();

    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/valid");

    let all_files = find_test_files(&corpus_dir);
    let test_files: Vec<_> = all_files
        .into_iter()
        .filter(|p| {
            if let Some(ref pat) = filter {
                p.to_string_lossy().contains(pat.as_str())
            } else {
                true
            }
        })
        .collect();

    eprintln!(
        "update_valid_corpus: {} files to process (dry_run={})",
        test_files.len(),
        dry_run
    );

    let mut updated = 0;
    let mut unchanged = 0;
    let mut errors = 0;
    let mut skipped = 0;

    for (i, test_file) in test_files.iter().enumerate() {
        let content = match fs::read_to_string(test_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ERROR reading {}: {}", test_file.display(), e);
                errors += 1;
                continue;
            }
        };

        let relative = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ERROR parsing {}: {}", relative.display(), e);
                errors += 1;
                continue;
            }
        };

        let (directive_line, source) = extract_parts(&content);

        // Valid corpus pipeline: parse + AST display + typecheck warnings
        let (output, error) = match tinct::parse(&test.input) {
            Ok(_) => {
                if test.expectations.out.is_some() {
                    match tinct::parse_surface_expression(&test.input) {
                        Ok(surface_node) => (Some(format!("{}", surface_node)), None),
                        Err(e) => (None, Some(format!("{e}"))),
                    }
                } else {
                    (None, None)
                }
            }
            Err(e) => (None, Some(format!("{e}"))),
        };

        let warnings = match tinct::typecheck_source_errors_only(&test.input) {
            Ok(()) => None,
            Err(e) => Some(e),
        };

        // Regression guard: if the existing file expected success (=== out) but parsing
        // now produces an error, do NOT overwrite it — this would silently lock in a
        // regression as "expected behavior".
        if test.expectations.out.is_some() && error.is_some() {
            eprintln!(
                "  SKIP: previously-passing test now produces error — possible regression: {}",
                relative.display()
            );
            skipped += 1;
            continue;
        }

        let new_content = rebuild_test_file(
            directive_line.as_deref(),
            &source,
            output.as_deref().map(|s| s.trim()),
            error.as_deref(),
            warnings.as_deref(),
        );

        if new_content == content {
            unchanged += 1;
        } else {
            updated += 1;
            if dry_run {
                eprintln!(
                    "  [{}/{}] WOULD UPDATE: {}",
                    i + 1,
                    test_files.len(),
                    relative.display()
                );
            } else {
                if (i + 1) % 50 == 0 || i == 0 {
                    eprintln!(
                        "  [{}/{}] updating: {}",
                        i + 1,
                        test_files.len(),
                        relative.display()
                    );
                }
                fs::write(test_file, &new_content).unwrap_or_else(|e| {
                    eprintln!("  ERROR writing {}: {}", test_file.display(), e);
                });
            }
        }
    }

    eprintln!("\n=== Valid Corpus Summary ===");
    eprintln!("  Total:     {}", test_files.len());
    eprintln!("  Updated:   {}", updated);
    eprintln!("  Unchanged: {}", unchanged);
    eprintln!("  Skipped:   {}", skipped);
    eprintln!("  Errors:    {}", errors);
}

/// Update the typecheck warnings corpus.
#[test]
#[ignore]
fn update_typecheck_warnings_corpus() {
    let dry_run = std::env::var("UPDATE_CORPUS_DRY_RUN").is_ok_and(|v| v == "1" || v == "true");
    let filter = std::env::var("UPDATE_CORPUS_FILTER").ok();

    let corpus_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/typecheck/warnings");

    let all_files = find_test_files(&corpus_dir);
    let test_files: Vec<_> = all_files
        .into_iter()
        .filter(|p| {
            if let Some(ref pat) = filter {
                p.to_string_lossy().contains(pat.as_str())
            } else {
                true
            }
        })
        .collect();

    eprintln!(
        "update_typecheck_warnings_corpus: {} files to process (dry_run={})",
        test_files.len(),
        dry_run
    );

    let mut updated = 0;
    let mut unchanged = 0;
    let mut errors = 0;
    let mut skipped = 0;

    for (i, test_file) in test_files.iter().enumerate() {
        let content = match fs::read_to_string(test_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ERROR reading {}: {}", test_file.display(), e);
                errors += 1;
                continue;
            }
        };

        let relative = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ERROR parsing {}: {}", relative.display(), e);
                errors += 1;
                continue;
            }
        };

        let (directive_line, source) = extract_parts(&content);

        // Typecheck warnings corpus: eval for output + typecheck for warnings
        tinct::clear_stdlib_cache();

        let eval_result = if test.cap_net.is_empty() {
            tinct::eval_source_with_config(&test.input, test.no_fs)
        } else {
            tinct::eval_source_with_cap_net(&test.input, test.no_fs, &test.cap_net)
        };

        let (output, eval_error) = match eval_result {
            Ok(actual) => (Some(actual), None),
            Err(e) => (None, Some(format!("{e}"))),
        };

        // Regression guard: if the existing file expected success (=== out) but eval now
        // produces an error, do NOT overwrite it — this would silently drop a passing
        // test's expected output and lock in a regression.
        if test.expectations.out.is_some() && eval_error.is_some() {
            eprintln!(
                "  SKIP: previously-passing test now produces error — possible regression: {}",
                relative.display()
            );
            eprintln!(
                "    eval error: {}",
                truncate(eval_error.as_deref().unwrap_or(""), 100)
            );
            skipped += 1;
            continue;
        }

        if eval_error.is_some() {
            eprintln!(
                "  WARN: {} eval failed: {}",
                relative.display(),
                truncate(eval_error.as_deref().unwrap_or(""), 100)
            );
        }

        let warnings = match tinct::typecheck_source(&test.input) {
            Ok(()) => None,
            Err(e) => Some(e),
        };

        let new_content = rebuild_test_file(
            directive_line.as_deref(),
            &source,
            output.as_deref().map(|s| s.trim()),
            None, // typecheck/warnings tests don't have error sections
            warnings.as_deref(),
        );

        if new_content == content {
            unchanged += 1;
        } else {
            updated += 1;
            if dry_run {
                eprintln!(
                    "  [{}/{}] WOULD UPDATE: {}",
                    i + 1,
                    test_files.len(),
                    relative.display()
                );
            } else {
                if (i + 1) % 50 == 0 || i == 0 {
                    eprintln!(
                        "  [{}/{}] updating: {}",
                        i + 1,
                        test_files.len(),
                        relative.display()
                    );
                }
                fs::write(test_file, &new_content).unwrap_or_else(|e| {
                    eprintln!("  ERROR writing {}: {}", test_file.display(), e);
                });
            }
        }
    }

    eprintln!("\n=== Typecheck Warnings Corpus Summary ===");
    eprintln!("  Total:     {}", test_files.len());
    eprintln!("  Updated:   {}", updated);
    eprintln!("  Unchanged: {}", unchanged);
    eprintln!("  Skipped:   {}", skipped);
    eprintln!("  Errors:    {}", errors);
}
