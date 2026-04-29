use std::fs;
use std::path::{Path, PathBuf};
use tinct::{eval_source, eval_source_with_config, parse, parse_expression, typecheck_source};

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

/// Parsed test file with optional directives.
struct TestFile<'a> {
    /// The LLT source code to evaluate (directives stripped).
    input: &'a str,
    /// Expected output or error substring (if `===` delimiter present).
    expected: Option<&'a str>,
    /// Whether to enable `--no-fs` mode (from `# no_fs` directive).
    no_fs: bool,
    /// Whether this is an error test with substring matching (from `=== ERROR:` prefix).
    is_error_substring: bool,
}

/// Split a test file on `===` delimiter. Returns (input, Option<expected>).
/// Uses `===` instead of `---` because `---` is a valid LLT document separator.
///
/// Supports directives on the first line:
/// - `# no_fs` — evaluate with filesystem access disabled (`no_fs: true`)
///
/// IMPORTANT: If the first line starts with `#`, it is treated as a directive line
/// and is STRIPPED from the input before evaluation. This means `#`-prefixed content
/// on line 1 is never evaluated, even if it's just a comment.
///
/// Note: Expected output (the section after `===`) depends on the test runner:
/// - Valid corpus: compares first expression's AST (via `parse_expression()`).
/// - Eval corpus: compares full file evaluation (last expression of last document).
fn split_test_file(content: &str) -> TestFile {
    const DELIM: &str = "===";
    const NEWLINE_DELIM_NEWLINE: &str = "\n===\n";
    const NEWLINE_DELIM: &str = "\n===";
    const ERROR_PREFIX: &str = "ERROR:";

    // Check for directives on the first line
    let (directives_line, rest) = if let Some(newline_pos) = content.find('\n') {
        let (first_line, remainder) = content.split_at(newline_pos);
        if first_line.trim().starts_with('#') {
            (first_line.trim(), &remainder[1..]) // skip the newline
        } else {
            ("", content)
        }
    } else {
        ("", content)
    };

    // Check if "no_fs" is the only directive (after the leading #)
    // This avoids false positives on comments containing "no_fs" as a word
    let no_fs = directives_line
        .strip_prefix('#')
        .map(|s| s.trim())
        .map_or(false, |s| s == "no_fs");
    let content = rest;

    if let Some(pos) = content.find(NEWLINE_DELIM_NEWLINE) {
        let (input, rest) = content.split_at(pos + 1); // include trailing newline before delimiter
        let expected = &rest[DELIM.len() + 1..]; // skip "===\n"
        let trimmed = expected.trim();

        // Check if expected output starts with "ERROR:" prefix
        let (is_error_substring, final_expected) =
            if let Some(stripped) = trimmed.strip_prefix(ERROR_PREFIX) {
                (true, stripped.trim())
            } else {
                (false, trimmed)
            };

        TestFile {
            input,
            expected: Some(final_expected),
            no_fs,
            is_error_substring,
        }
    } else if let Some(pos) = content.find(NEWLINE_DELIM) {
        // === at end of file with no trailing newline
        let (input, rest) = content.split_at(pos + 1);
        let expected = &rest[DELIM.len()..];
        let trimmed = expected.trim();

        // Check if expected output starts with "ERROR:" prefix
        let (is_error_substring, final_expected) =
            if let Some(stripped) = trimmed.strip_prefix(ERROR_PREFIX) {
                (true, stripped.trim())
            } else {
                (false, trimmed)
            };

        TestFile {
            input,
            expected: if final_expected.is_empty() {
                None
            } else {
                Some(final_expected)
            },
            no_fs,
            is_error_substring,
        }
    } else {
        TestFile {
            input: content,
            expected: None,
            no_fs,
            is_error_substring: false,
        }
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

        let test = split_test_file(&content);

        let expected_output = match test.expected {
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
        match parse(test.input) {
            Ok(_) => {
                // Expected output is compared against single-expression format
                match parse_expression(test.input) {
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

        let test = split_test_file(&content);

        let expected_substr = match test.expected {
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

        match parse(test.input) {
            Ok(_) => failed.push((
                relative_path.to_path_buf(),
                "Expected parse to fail".to_string(),
            )),
            Err(e) => {
                let error_msg = format!("{}", e);
                // Both ERROR: prefix and non-prefix tests use substring matching.
                // The is_error_substring field is available for future extensions.
                if !error_msg.contains(expected_substr) {
                    let match_type = if test.is_error_substring {
                        "expected substring (ERROR: prefix)"
                    } else {
                        "expected substring"
                    };
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- {} ---\n{}\n--- actual error ---\n{}",
                            match_type, expected_substr, error_msg
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
        // Eval corpus
        "tests/corpus/eval",
        "tests/corpus/eval/access",
        "tests/corpus/eval/builtins",
        "tests/corpus/eval/cross_feature",
        "tests/corpus/eval/errors",
        "tests/corpus/eval/functions",
        "tests/corpus/eval/laziness",
        "tests/corpus/eval/regressions",
        "tests/corpus/eval/stdlib",
        "tests/corpus/eval/type_assertions",
        "tests/corpus/eval/typecheck",
        "tests/corpus/eval/underscore",
        // Invalid corpus
        "tests/corpus/invalid/syntax_errors",
        // Valid corpus
        "tests/corpus/valid/access",
        "tests/corpus/valid/annotations",
        "tests/corpus/valid/complex",
        "tests/corpus/valid/documents",
        "tests/corpus/valid/edge_cases",
        "tests/corpus/valid/literals",
        "tests/corpus/valid/simple",
        "tests/corpus/valid/special_forms",
    ];

    for dir in &required_dirs {
        let path = manifest_dir.join(dir);
        assert!(path.exists(), "Required test directory missing: {}", dir);
        assert!(path.is_dir(), "Path is not a directory: {}", dir);
    }

    // Minimum test count assertions for key directories
    const EVAL_LAZINESS_MIN: usize = 15;
    const EVAL_BUILTINS_MIN: usize = 87;
    const EVAL_STDLIB_MIN: usize = 133;
    const EVAL_ERRORS_MIN: usize = 45;

    let laziness_count = find_test_files(&manifest_dir.join("tests/corpus/eval/laziness")).len();
    assert!(
        laziness_count >= EVAL_LAZINESS_MIN,
        "tests/corpus/eval/laziness/ has {} tests, expected at least {}",
        laziness_count,
        EVAL_LAZINESS_MIN
    );

    let builtins_count = find_test_files(&manifest_dir.join("tests/corpus/eval/builtins")).len();
    assert!(
        builtins_count >= EVAL_BUILTINS_MIN,
        "tests/corpus/eval/builtins/ has {} tests, expected at least {}",
        builtins_count,
        EVAL_BUILTINS_MIN
    );

    let stdlib_count = find_test_files(&manifest_dir.join("tests/corpus/eval/stdlib")).len();
    assert!(
        stdlib_count >= EVAL_STDLIB_MIN,
        "tests/corpus/eval/stdlib/ has {} tests, expected at least {}",
        stdlib_count,
        EVAL_STDLIB_MIN
    );

    let errors_count = find_test_files(&manifest_dir.join("tests/corpus/eval/errors")).len();
    assert!(
        errors_count >= EVAL_ERRORS_MIN,
        "tests/corpus/eval/errors/ has {} tests, expected at least {}",
        errors_count,
        EVAL_ERRORS_MIN
    );
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

        let test = split_test_file(&content);

        let expected_output = match test.expected {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "eval corpus test file missing expected output after ===".to_string(),
                ));
                continue;
            }
        };

        match eval_source(test.input) {
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

        let test = split_test_file(&content);

        let expected_substr = match test.expected {
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

        // Evaluate in a thread with larger stack to handle depth-exceeded tests
        // (e.g., typeassert_depth_exceeded_not_circular.llt-eval) which can overflow
        // Rust's default test thread stack when evaluating deeply recursive code.
        let input = test.input.to_string();
        let no_fs = test.no_fs;
        let eval_result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // 16MB stack
            .spawn(move || eval_source_with_config(&input, no_fs))
            .unwrap()
            .join()
            .unwrap();

        match eval_result {
            Ok(actual) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("Expected eval to fail, but got success: {}", actual.trim()),
                ));
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                // Both ERROR: prefix and non-prefix tests use substring matching.
                // The is_error_substring field is available for future extensions
                // (e.g., exact error code validation, span checking).
                if !error_msg.contains(expected_substr) {
                    let match_type = if test.is_error_substring {
                        "expected substring (ERROR: prefix)"
                    } else {
                        "expected substring"
                    };
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- {} ---\n{}\n--- actual error ---\n{}",
                            match_type, expected_substr, error_msg
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

        let test = split_test_file(&content);

        // Evaluate in a thread with larger stack to handle depth-exceeded tests
        // (e.g., typeassert_depth_exceeded_not_circular.llt-eval) which can overflow
        // Rust's default test thread stack when evaluating deeply recursive code.
        let input = test.input.to_string();
        let no_fs = test.no_fs;
        let eval_result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // 16MB stack
            .spawn(move || eval_source_with_config(&input, no_fs))
            .unwrap()
            .join()
            .unwrap();

        match eval_result {
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
///
/// IMPORTANT: This function matches exactly 3 digits ([E\d\d\d]).
/// All LLT error codes use the 3-digit format (E001-E999).
/// If the error code format changes, update this function.
fn has_error_code_prefix(error_msg: &str) -> bool {
    // Look for pattern [EXXX] where XXX are exactly three digits
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
    if test_files.is_empty() {
        return;
    }

    let mut failed = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = split_test_file(&content);

        // Type check should succeed for all files in tests/corpus/eval/typecheck/
        match typecheck_source(test.input) {
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

        let test = split_test_file(&content);

        let expected_substr = match test.expected {
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
        match typecheck_source(test.input) {
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

// ---------------------------------------------------------------------------
// Unit tests for split_test_file()
// ---------------------------------------------------------------------------

#[test]
fn test_split_test_file_no_fs_directive() {
    let content = "# no_fs\n[call $include \"file.llt\"]\n===\nfilesystem access is disabled";
    let test = split_test_file(content);
    assert_eq!(test.input, "[call $include \"file.llt\"]\n");
    assert_eq!(test.expected, Some("filesystem access is disabled"));
    assert!(test.no_fs, "no_fs directive should be detected");
}

#[test]
fn test_split_test_file_no_fs_substring_false_positive() {
    let content = "# testing no_fs filesystem semantics\n[x: 1]\n===\n[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expected, Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for substring match 'no_fs'"
    );
}

#[test]
fn test_split_test_file_no_fs_prefix_false_positive() {
    let content = "# no_fs_path\n[x: 1]\n===\n[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expected, Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for token 'no_fs_path'"
    );
}

#[test]
fn test_split_test_file_no_directive() {
    let content = "[x: 1 y: 2]\n===\n[\"x\": 1  \"y\": 2]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1 y: 2]\n");
    assert_eq!(test.expected, Some("[\"x\": 1  \"y\": 2]"));
    assert!(!test.no_fs, "no_fs should default to false");
}

#[test]
fn test_split_test_file_eof_without_trailing_newline() {
    let content = "[x: 1]\n===[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expected, Some("[\"x\": 1]"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_missing_delimiter() {
    let content = "[x: 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]");
    assert_eq!(test.expected, None);
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_delimiter_in_expected() {
    let content = "[x: 1]\n===\n[\"x\": 1]  # comment with === in it";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expected, Some("[\"x\": 1]  # comment with === in it"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_error_prefix() {
    let content = "[call $error \"boom\"]\n===\nERROR: [E024]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[call $error \"boom\"]\n");
    assert_eq!(test.expected, Some("[E024]"));
    assert!(test.is_error_substring, "ERROR: prefix should be detected");
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_error_prefix_with_whitespace() {
    let content = "[call $error \"boom\"]\n===\n  ERROR:   [E024] something  ";
    let test = split_test_file(content);
    assert_eq!(test.input, "[call $error \"boom\"]\n");
    assert_eq!(test.expected, Some("[E024] something"));
    assert!(test.is_error_substring, "ERROR: prefix should be detected");
}

#[test]
fn test_split_test_file_empty_content() {
    let content = "";
    let test = split_test_file(content);
    assert_eq!(test.input, "");
    assert_eq!(test.expected, None);
    assert!(!test.no_fs);
    assert!(!test.is_error_substring);
}

#[test]
fn test_split_test_file_whitespace_around_delimiter() {
    // Delimiter must be exactly "\n===\n" or "\n===" (no surrounding whitespace).
    // If there's whitespace, the delimiter is not recognized.
    let content = "[x: 1]  \n  ===  \n  [\"x\": 1]  ";
    let test = split_test_file(content);
    // Since "  ===" doesn't match "\n===", the entire content is treated as input
    assert_eq!(test.input, "[x: 1]  \n  ===  \n  [\"x\": 1]  ");
    assert_eq!(test.expected, None);
    assert!(!test.no_fs);
}

// ---------------------------------------------------------------------------
// Unit tests for has_error_code_prefix()
// ---------------------------------------------------------------------------

#[test]
fn test_has_error_code_prefix_valid_e001() {
    assert!(
        has_error_code_prefix("[E001] some error"),
        "[E001] should be detected"
    );
}

#[test]
fn test_has_error_code_prefix_valid_e999() {
    assert!(
        has_error_code_prefix("[E999] another error"),
        "[E999] should be detected"
    );
}

#[test]
fn test_has_error_code_prefix_valid_in_middle() {
    assert!(
        has_error_code_prefix("Error: [E042] invalid operation"),
        "[E042] in the middle should be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_no_brackets() {
    assert!(
        !has_error_code_prefix("E001 no brackets"),
        "E001 without brackets should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_two_digits() {
    assert!(
        !has_error_code_prefix("[E01] two digits only"),
        "[E01] with only 2 digits should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_four_digits() {
    assert!(
        !has_error_code_prefix("[E0001] four digits"),
        "[E0001] with 4 digits should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_empty_string() {
    assert!(
        !has_error_code_prefix(""),
        "empty string should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_no_code() {
    assert!(
        !has_error_code_prefix("no error code here"),
        "string without error code should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_lowercase_e() {
    assert!(
        !has_error_code_prefix("[e001] lowercase e"),
        "[e001] with lowercase 'e' should NOT be detected"
    );
}

#[test]
fn test_has_error_code_prefix_invalid_letters_in_number() {
    assert!(
        !has_error_code_prefix("[E0A1] letter in number"),
        "[E0A1] with letter in number should NOT be detected"
    );
}

/// Documents the behavior of split_test_file() when `===` appears in expected output.
///
/// split_test_file splits at the FIRST `\n===\n` in the file (between input and expected).
/// Any subsequent `===` lines in the expected section are passed through verbatim — they are
/// NOT treated as additional delimiters. This means `===` can safely appear in expected output.
///
/// The practical limitation is that `\n===\n` in the INPUT section would cause a premature
/// split before the intended delimiter. In practice this never arises since LLT source doesn't
/// contain bare `===` lines.
#[test]
fn test_split_test_file_delimiter_limitation_documented() {
    // Construct a file where the expected output itself contains `===` on its own line.
    // split_test_file splits only at the FIRST `\n===\n` delimiter.
    let content = "expr\n===\nexpected line 1\n===\nexpected line 2\n";
    let parsed = split_test_file(content);
    // split_test_file splits only at the FIRST `===`. The second `===` is passed through
    // verbatim as part of the expected output — it is NOT treated as a second delimiter.
    // So the full expected output is "expected line 1\n===\nexpected line 2".
    assert_eq!(
        parsed.expected,
        Some("expected line 1\n===\nexpected line 2"),
        "split_test_file keeps the second `===` as literal expected content; \
         if any corpus test had `===` as the ONLY content expected, no special handling is needed"
    );
}

// ---------------------------------------------------------------------------
// Parser equivalence test — validates parser2 matches pest parser output
// ---------------------------------------------------------------------------

/// Test that parser2 produces identical ASTs to the pest parser on all corpus files.
///
/// This is the validation gate for the parser rewrite: if this test passes, parser2
/// is ready for the pest cutover. The test compares AST structure (ignoring spans
/// and comment maps) on every valid input file in tests/corpus/eval/.
#[test]
fn test_parser2_equivalence() {
    use tinct::parser2;

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
    let mut skipped = Vec::new();

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = split_test_file(&content);
        let input = test.input;

        // Parse with both parsers
        let pest_result = parse(input);
        let parser2_result = parser2::parse2(input);

        match (pest_result, parser2_result) {
            (Ok(pest_ast), Ok(parser2_output)) => {
                // Compare AST structure (ignore spans and comment maps)
                if !files_equal(&pest_ast.node, &parser2_output.file.node) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "AST mismatch\n--- pest ---\n{:#?}\n--- parser2 ---\n{:#?}",
                            pest_ast.node, parser2_output.file.node
                        ),
                    ));
                }
            }
            (Ok(_), Err(e)) => {
                // pest succeeded, parser2 failed
                failed.push((
                    relative_path.to_path_buf(),
                    format!("pest succeeded but parser2 failed: {}", e),
                ));
            }
            (Err(pest_err), Ok(_)) => {
                // pest failed, parser2 succeeded
                failed.push((
                    relative_path.to_path_buf(),
                    format!(
                        "pest failed but parser2 succeeded (pest error: {})",
                        pest_err
                    ),
                ));
            }
            (Err(pest_err), Err(parser2_err)) => {
                // Both failed - compare error messages to ensure consistency
                // Normalize error messages by extracting the core error kind
                let pest_msg = pest_err.message.to_lowercase();
                let parser2_msg = parser2_err.message.to_lowercase();

                // Check if error messages indicate the same kind of error
                // Allow for different phrasing but same error category
                let both_depth = pest_msg.contains("depth") && parser2_msg.contains("depth");
                let both_duplicate =
                    pest_msg.contains("duplicate") && parser2_msg.contains("duplicate");
                let both_unclosed = (pest_msg.contains("unclosed")
                    || pest_msg.contains("unmatched"))
                    && (parser2_msg.contains("unclosed") || parser2_msg.contains("unmatched"));
                let both_unexpected =
                    pest_msg.contains("unexpected") && parser2_msg.contains("unexpected");
                let both_invalid = pest_msg.contains("invalid") && parser2_msg.contains("invalid");

                if !(both_depth
                    || both_duplicate
                    || both_unclosed
                    || both_unexpected
                    || both_invalid)
                {
                    // Error kinds don't match - this is a divergence
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Both parsers failed but with different error kinds:\npest: {}\nparser2: {}",
                            pest_err.message, parser2_err.message
                        ),
                    ));
                } else {
                    // Errors are similar - skip this file
                    skipped.push(relative_path.to_path_buf());
                }
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} parser2 equivalence test(s) failed:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        if !skipped.is_empty() {
            eprintln!("\n{} file(s) skipped (both parsers failed):", skipped.len());
            for path in &skipped {
                eprintln!("  - {}", path.display());
            }
        }
        panic!("Parser2 equivalence tests failed");
    }
}

/// Compare two File AST nodes for structural equality (ignoring spans and resolved types).
fn files_equal(a: &tinct::ast::File, b: &tinct::ast::File) -> bool {
    if a.documents.len() != b.documents.len() {
        return false;
    }
    a.documents
        .iter()
        .zip(b.documents.iter())
        .all(|(da, db)| documents_equal(&da.node, &db.node))
}

/// Compare two Document AST nodes for structural equality.
fn documents_equal(a: &tinct::ast::Document, b: &tinct::ast::Document) -> bool {
    if a.expressions.len() != b.expressions.len() {
        return false;
    }
    a.expressions
        .iter()
        .zip(b.expressions.iter())
        .all(|(ea, eb)| exprs_equal(&ea.node, &eb.node))
}

/// Compare two Expr AST nodes for structural equality (ignoring spans and resolved types).
fn exprs_equal(a: &tinct::ast::Expr, b: &tinct::ast::Expr) -> bool {
    use tinct::ast::Expr;

    match (a, b) {
        (Expr::Int(n1), Expr::Int(n2)) => n1 == n2,
        (Expr::Float(f1), Expr::Float(f2)) => f1 == f2,
        (Expr::Bool(b1), Expr::Bool(b2)) => b1 == b2,
        (Expr::Str(s1), Expr::Str(s2)) => s1 == s2,
        (Expr::VarRef(v1), Expr::VarRef(v2)) => v1 == v2,
        (
            Expr::DotAccess {
                expr: e1,
                field: f1,
            },
            Expr::DotAccess {
                expr: e2,
                field: f2,
            },
        ) => f1 == f2 && exprs_equal(&e1.node, &e2.node),
        (Expr::BracketAccess { expr: e1, key: k1 }, Expr::BracketAccess { expr: e2, key: k2 }) => {
            exprs_equal(&e1.node, &e2.node) && exprs_equal(&k1.node, &k2.node)
        }
        (
            Expr::RangeAccess {
                expr: e1,
                start: s1,
                end: end1,
            },
            Expr::RangeAccess {
                expr: e2,
                start: s2,
                end: end2,
            },
        ) => {
            exprs_equal(&e1.node, &e2.node)
                && opt_exprs_equal(s1.as_ref().map(|v| &**v), s2.as_ref().map(|v| &**v))
                && opt_exprs_equal(end1.as_ref().map(|v| &**v), end2.as_ref().map(|v| &**v))
        }
        (Expr::Dict(entries1), Expr::Dict(entries2)) => {
            if entries1.len() != entries2.len() {
                return false;
            }
            entries1
                .iter()
                .zip(entries2.iter())
                .all(|(e1, e2)| entries_equal(&e1.node, &e2.node))
        }
        (
            Expr::Call {
                func: f1,
                args: a1,
                named_args: n1,
            },
            Expr::Call {
                func: f2,
                args: a2,
                named_args: n2,
            },
        ) => {
            exprs_equal(&f1.node, &f2.node)
                && a1.len() == a2.len()
                && a1
                    .iter()
                    .zip(a2.iter())
                    .all(|(e1, e2)| exprs_equal(&e1.node, &e2.node))
                && n1.len() == n2.len()
                && n1
                    .iter()
                    .zip(n2.iter())
                    .all(|(na1, na2)| named_args_equal(&na1.node, &na2.node))
        }
        (
            Expr::Fn {
                return_ann: r1,
                params: p1,
                body: b1,
                desugared: d1,
            },
            Expr::Fn {
                return_ann: r2,
                params: p2,
                body: b2,
                desugared: d2,
            },
        ) => {
            d1 == d2
                && opt_annotations_equal(r1.as_ref(), r2.as_ref())
                && p1.len() == p2.len()
                && p1
                    .iter()
                    .zip(p2.iter())
                    .all(|(pa1, pa2)| params_equal(&pa1.node, &pa2.node))
                && exprs_equal(&b1.node, &b2.node)
        }
        (Expr::TypeAlias(t1), Expr::TypeAlias(t2)) => exprs_equal(&t1.node, &t2.node),
        (
            Expr::TypeAssert {
                annotation: a1,
                expr: e1,
                ..
            },
            Expr::TypeAssert {
                annotation: a2,
                expr: e2,
                ..
            },
        ) => {
            // Ignore resolved_type (elaboration data)
            annotations_equal(&a1.node, &a2.node) && exprs_equal(&e1.node, &e2.node)
        }
        (
            Expr::Annotated {
                name: n1,
                annotation: a1,
            },
            Expr::Annotated {
                name: n2,
                annotation: a2,
            },
        ) => n1 == n2 && annotations_equal(&a1.node, &a2.node),
        (Expr::Rest(r1), Expr::Rest(r2)) => r1 == r2,
        _ => false,
    }
}

fn opt_exprs_equal(
    a: Option<&tinct::ast::Spanned<tinct::ast::Expr>>,
    b: Option<&tinct::ast::Spanned<tinct::ast::Expr>>,
) -> bool {
    match (a, b) {
        (Some(e1), Some(e2)) => exprs_equal(&e1.node, &e2.node),
        (None, None) => true,
        _ => false,
    }
}

fn entries_equal(a: &tinct::ast::Entry, b: &tinct::ast::Entry) -> bool {
    opt_exprs_equal(a.key.as_ref(), b.key.as_ref()) && exprs_equal(&a.value.node, &b.value.node)
}

fn named_args_equal(a: &tinct::ast::NamedArg, b: &tinct::ast::NamedArg) -> bool {
    a.name == b.name && exprs_equal(&a.value.node, &b.value.node)
}

fn params_equal(a: &tinct::ast::Param, b: &tinct::ast::Param) -> bool {
    a.name == b.name
        && a.variadic == b.variadic
        && opt_annotations_equal(a.annotation.as_ref(), b.annotation.as_ref())
}

fn opt_annotations_equal(
    a: Option<&tinct::ast::Spanned<tinct::ast::Annotation>>,
    b: Option<&tinct::ast::Spanned<tinct::ast::Annotation>>,
) -> bool {
    match (a, b) {
        (Some(ann1), Some(ann2)) => annotations_equal(&ann1.node, &ann2.node),
        (None, None) => true,
        _ => false,
    }
}

fn annotations_equal(a: &tinct::ast::Annotation, b: &tinct::ast::Annotation) -> bool {
    use tinct::ast::Annotation;

    match (a, b) {
        (Annotation::Simple(s1), Annotation::Simple(s2)) => s1 == s2,
        (Annotation::PropertyDict(entries1), Annotation::PropertyDict(entries2)) => {
            if entries1.len() != entries2.len() {
                return false;
            }
            entries1
                .iter()
                .zip(entries2.iter())
                .all(|(e1, e2)| entries_equal(&e1.node, &e2.node))
        }
        _ => false,
    }
}
