use lazy_lisp_transformer::{eval_source, parse, parse_expression};
use std::fs;
use std::path::{Path, PathBuf};

/// Recursively find all .txt files in a directory
fn find_test_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let entry = entry.expect("failed to read corpus directory entry");
            let path = entry.path();
            if path.is_dir() {
                files.extend(find_test_files(&path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

/// Split a test file on `===` delimiter. Returns (input, Option<expected>).
/// Uses `===` instead of `---` because `---` is a valid LLT document separator.
///
/// Note: Expected output (the section after `===`) is compared against the result
/// of `parse_expression()`, which returns the LAST expression from the FIRST document.
/// For single-expression files, this is straightforward. For multi-expression or
/// multi-document files, only the final expression of the first document is compared.
fn split_test_file(content: &str) -> (&str, Option<&str>) {
    const DELIM: &str = "===";
    const NEWLINE_DELIM_NEWLINE: &str = "\n===\n";
    const NEWLINE_DELIM: &str = "\n===";

    if let Some(pos) = content.find(NEWLINE_DELIM_NEWLINE) {
        let (input, rest) = content.split_at(pos + 1); // include trailing newline before delimiter
        let expected = &rest[DELIM.len() + 1..]; // skip "===\n"
        (input, Some(expected.trim()))
    } else if let Some(pos) = content.find(NEWLINE_DELIM) {
        // === at end of file with no trailing newline
        let (input, rest) = content.split_at(pos + 1);
        let expected = &rest[DELIM.len()..];
        let trimmed = expected.trim();
        (
            input,
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            },
        )
    } else {
        (content, None)
    }
}

#[test]
fn test_valid_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/valid");

    let test_files = find_test_files(&corpus_dir);
    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    let mut failed = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let (input, expected) = split_test_file(&content);

        let expected_output = match expected {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "valid corpus test file missing expected AST output after ===".to_string(),
                ));
                continue;
            }
        };

        // Use parse (full file) to verify the input is valid.
        // For expected output comparison, use parse_expression (single expr).
        match parse(input) {
            Ok(_) => {
                // Expected output is compared against single-expression format
                match parse_expression(input) {
                    Ok(ast) => {
                        let actual = format!("{}", ast.node);
                        if actual.trim() != expected_output {
                            failed.push((
                                relative_path.to_path_buf(),
                                format!(
                                    "AST mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
                                    expected_output,
                                    actual.trim()
                                ),
                            ));
                        }
                    }
                    Err(e) => {
                        failed.push((relative_path.to_path_buf(), format!("parse error: {e}")))
                    }
                }
            }
            Err(e) => failed.push((relative_path.to_path_buf(), format!("parse error: {e}"))),
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} valid test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Valid corpus tests failed");
    }
}

#[test]
fn test_invalid_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/invalid");

    let test_files = find_test_files(&corpus_dir);
    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    let mut failed = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let (input, expected) = split_test_file(&content);

        let expected_substr = match expected {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "invalid corpus test file missing expected error substring after ==="
                        .to_string(),
                ));
                continue;
            }
        };

        match parse(input) {
            Ok(_) => failed.push((
                relative_path.to_path_buf(),
                "Expected parse to fail".to_string(),
            )),
            Err(e) => {
                let error_msg = format!("{}", e);
                if !error_msg.contains(expected_substr) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- expected substring ---\n{}\n--- actual error ---\n{}",
                            expected_substr, error_msg
                        ),
                    ));
                }
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} invalid test(s) incorrectly parsed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Invalid corpus tests failed");
    }
}

#[test]
fn test_corpus_structure() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let required_dirs = [
        "tests/corpus/valid/simple",
        "tests/corpus/valid/complex",
        "tests/corpus/valid/edge_cases",
        "tests/corpus/invalid/syntax_errors",
        "tests/corpus/eval",
        "tests/corpus/eval/errors",
    ];

    for dir in &required_dirs {
        let path = manifest_dir.join(dir);
        assert!(path.exists(), "Required test directory missing: {}", dir);
        assert!(path.is_dir(), "Path is not a directory: {}", dir);
    }
}

#[test]
fn test_eval_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval");
    let errors_dir = corpus_dir.join("errors");

    let test_files: Vec<_> = find_test_files(&corpus_dir)
        .into_iter()
        .filter(|p| !p.starts_with(&errors_dir))
        .collect();
    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    let mut failed = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let (input, expected) = split_test_file(&content);

        let expected_output = match expected {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "eval corpus test file missing expected output after ===".to_string(),
                ));
                continue;
            }
        };

        match eval_source(input) {
            Ok(actual) => {
                if actual.trim() != expected_output {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "eval output mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
                            expected_output,
                            actual.trim()
                        ),
                    ));
                }
            }
            Err(e) => {
                failed.push((relative_path.to_path_buf(), format!("eval error: {e}")));
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} eval test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Eval corpus tests failed");
    }
}

#[test]
fn test_eval_error_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval/errors");

    let test_files = find_test_files(&corpus_dir);
    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    let mut failed = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let (input, expected) = split_test_file(&content);

        let expected_substr = match expected {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "eval error corpus test file missing expected error substring after ==="
                        .to_string(),
                ));
                continue;
            }
        };

        match eval_source(input) {
            Ok(actual) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("Expected eval to fail, but got success: {}", actual.trim()),
                ));
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                if !error_msg.contains(expected_substr) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- expected substring ---\n{}\n--- actual error ---\n{}",
                            expected_substr, error_msg
                        ),
                    ));
                }
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} eval error test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Eval error corpus tests failed");
    }
}
