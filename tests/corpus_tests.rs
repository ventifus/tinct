use std::fs;
use std::path::{Path, PathBuf};
use tinct::{eval_source, parse, parse_expression, typecheck_source};

/// Recursively find all .llt-eval files in a directory
fn find_test_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let entry = entry.expect("failed to read corpus directory entry");
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

#[test]
fn test_eval_error_corpus_has_error_codes() {
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

        let (input, _expected) = split_test_file(&content);

        // Evaluate and check if error contains an error code
        match eval_source(input) {
            Ok(actual) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!(
                        "Expected eval to fail with error code, but got success: {}",
                        actual.trim()
                    ),
                ));
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                // Check for error code pattern: [E0XX] where XX are digits
                if !has_error_code_prefix(&error_msg) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message missing [E0XX] error code prefix\n--- actual error ---\n{}",
                            error_msg
                        ),
                    ));
                }
            }
        }
    }

    if !failed.is_empty() {
        eprintln!(
            "\n{} eval error test(s) missing error code prefix:",
            failed.len()
        );
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Eval error corpus tests missing error codes");
    }
}

/// Check if error message contains an error code pattern like [E001], [E099], etc.
fn has_error_code_prefix(error_msg: &str) -> bool {
    // Look for pattern [EXXX] where XXX are three digits
    error_msg.chars().collect::<Vec<_>>().windows(6).any(|w| {
        w[0] == '['
            && w[1] == 'E'
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[4].is_ascii_digit()
            && w[5] == ']'
    })
}

/// Typecheck corpus runner — disabled until stdlib builtins have type signatures.
///
/// 4 of 8 corpus files use builtins (`$+`, `$merge`, `$get`) that are not registered
/// in `TypeEnv::new()`, causing spurious "undefined variable" type errors.
/// Re-enable once the `typecheck-stdlib-types` sprint lands (`TypeEnv::with_builtins()`).
/// See TODO.md §type-extensions — "Add `TypeEnv::with_builtins()` constructor".
#[test]
#[ignore = "stdlib builtins lack type signatures — re-enable once typecheck-stdlib-types sprint is complete"]
fn test_typecheck_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval/typecheck");

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

        let (input, _expected) = split_test_file(&content);

        // Type check should succeed for all files in tests/corpus/eval/typecheck/
        match typecheck_source(input) {
            Ok(()) => {
                // Success - this is expected
            }
            Err(error_msg) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!(
                        "Expected typecheck to succeed, but got error(s):\n{}",
                        error_msg
                    ),
                ));
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} typecheck test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Typecheck corpus tests failed");
    }
}

/// Type error corpus runner — disabled until stdlib builtins have type signatures.
///
/// Companion to `test_typecheck_corpus`: files in `tests/corpus/invalid/type_errors/`
/// are expected to fail type-checking with a specific error substring. Disabled because
/// the type checker's `TypeEnv` lacks stdlib builtin signatures, making any test that
/// exercises builtins unreliable. Re-enable together with `test_typecheck_corpus` once
/// the `typecheck-stdlib-types` sprint lands.
#[test]
#[ignore = "stdlib builtins lack type signatures — re-enable once typecheck-stdlib-types sprint is complete"]
fn test_typecheck_error_corpus() {
    let corpus_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/invalid/type_errors");

    // Only run this test if the directory exists
    if !corpus_dir.exists() {
        // Skip silently if the directory doesn't exist yet
        return;
    }

    let test_files = find_test_files(&corpus_dir);
    if test_files.is_empty() {
        // Directory exists but has no tests yet - this is fine
        return;
    }

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
                    "type error corpus test file missing expected error substring after ==="
                        .to_string(),
                ));
                continue;
            }
        };

        // Type check should fail for all files in tests/corpus/invalid/type_errors/
        match typecheck_source(input) {
            Ok(()) => {
                failed.push((
                    relative_path.to_path_buf(),
                    "Expected typecheck to fail, but it succeeded".to_string(),
                ));
            }
            Err(error_msg) => {
                // Check if the error message contains the expected substring
                if !error_msg.contains(expected_substr) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- expected substring ---\n{}\n--- actual errors ---\n{}",
                            expected_substr, error_msg
                        ),
                    ));
                }
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} type error test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Type error corpus tests failed");
    }
}
