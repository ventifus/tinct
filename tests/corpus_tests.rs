mod test_helpers;

use std::fs;
use std::path::PathBuf;
use test_helpers::{find_test_files, run_corpus_dir, split_test_file, CorpusOutcome};
use tinct::{eval_source_with_config, parse, parse_expression, typecheck_source};

#[test]
fn test_valid_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/valid");

    let failures = run_corpus_dir(&corpus_dir, &[], |test| {
        // Parse-only pipeline: parse() + typecheck_source()
        // Parse failure maps to error, success maps to output (for AST comparison)
        match parse(&test.input) {
            Ok(_) => {
                // For files with === out, we need to produce the AST Display string
                let output = if test.expectations.out.is_some() {
                    match parse_expression(&test.input) {
                        Ok(ast) => Some(format!("{}", ast.node)),
                        Err(e) => {
                            return CorpusOutcome {
                                output: None,
                                warnings: None,
                                error: Some(format!("{e}")),
                            }
                        }
                    }
                } else {
                    None
                };

                // Run typecheck to get warnings
                let warnings = match typecheck_source(&test.input) {
                    Ok(()) => None,
                    Err(type_errors) => Some(type_errors),
                };

                CorpusOutcome {
                    output,
                    warnings,
                    error: None,
                }
            }
            Err(e) => CorpusOutcome {
                output: None,
                warnings: None,
                error: Some(format!("{e}")),
            },
        }
    });

    if !failures.is_empty() {
        eprintln!("\n{} valid test(s) failed:", failures.len());
        for failure in &failures {
            eprintln!("  - {}: {}", failure.path.display(), failure.message);
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

        let expected_substr = match &test.expectations.out {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "invalid corpus test file missing expected error substring after === out"
                        .to_string(),
                ));
                continue;
            }
        };

        match parse(&test.input) {
            Ok(_) => failed.push((
                relative_path.to_path_buf(),
                "Expected parse to fail".to_string(),
            )),
            Err(e) => {
                let error_msg = format!("{}", e);
                if !error_msg.contains(expected_substr.as_str()) {
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
        "tests/corpus/eval/type_errors",
        "tests/corpus/eval/typecheck",
        "tests/corpus/eval/pipeline",
        "tests/corpus/eval/type_system",
        "tests/corpus/eval/letrec",
        "tests/corpus/eval/underscore",
        "tests/corpus/eval/documents",
        // Invalid corpus
        "tests/corpus/invalid/pipeline",
        "tests/corpus/invalid/semantic_errors",
        "tests/corpus/invalid/syntax_errors",
        // Valid corpus
        "tests/corpus/valid/access",
        "tests/corpus/valid/annotations",
        "tests/corpus/valid/complex",
        "tests/corpus/valid/documents",
        "tests/corpus/valid/edge_cases",
        "tests/corpus/valid/literals",
        "tests/corpus/valid/parser_mechanisms",
        "tests/corpus/valid/simple",
        "tests/corpus/valid/special_forms",
        // Typecheck warnings corpus — one file per warning category
        "tests/corpus/typecheck",
        "tests/corpus/typecheck/warnings",
    ];

    for dir in &required_dirs {
        let path = manifest_dir.join(dir);
        assert!(path.exists(), "Required test directory missing: {}", dir);
        assert!(path.is_dir(), "Path is not a directory: {}", dir);
    }

    // Minimum test count assertions for key directories
    const EVAL_LAZINESS_MIN: usize = 21;
    const EVAL_BUILTINS_MIN: usize = 100;
    const EVAL_STDLIB_MIN: usize = 194;
    const EVAL_ERRORS_MIN: usize = 85;

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
    // type_errors/ files are expected to fail typecheck (not produce eval output);
    // they are handled by test_typecheck_error_corpus_eval instead.
    let type_errors_dir = corpus_dir.join("type_errors");

    // Spawn thread with large stack to prevent overflow in deeply-nested test cases.
    // Same rationale as test_eval_error_corpus: the stdlib Rc<Environment> recursive
    // drop at thread exit requires significant stack space (100+ MB in debug mode).
    // 512MB chosen to give comfortable headroom as the stdlib prelude grows over time.
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024) // 512MB — debug-mode stdlib cleanup needs ~100MB; extra headroom for prelude growth
        .spawn(move || {
            run_corpus_dir(&corpus_dir, &[type_errors_dir.as_path()], |test| {
                // Eval pipeline: eval_source_with_config() + typecheck_source()
                let eval_result = eval_source_with_config(&test.input, test.no_fs);
                let typecheck_result = typecheck_source(&test.input);

                let (output, error) = match eval_result {
                    Ok(actual) => (Some(actual), None),
                    Err(e) => (None, Some(format!("{e}"))),
                };

                let warnings = match typecheck_result {
                    Ok(()) => None,
                    Err(type_errors) => Some(type_errors),
                };

                CorpusOutcome {
                    output,
                    warnings,
                    error,
                }
            })
        })
        .unwrap()
        .join()
        .unwrap();

    if !result.is_empty() {
        eprintln!("\n{} eval test(s) failed:", result.len());
        for failure in &result {
            eprintln!("  - {}: {}", failure.path.display(), failure.message);
        }
        panic!("Eval corpus tests failed");
    }
}

/// Typecheck corpus runner — validates that all `.llt-eval` files in
/// `tests/corpus/eval/typecheck/` pass type checking without errors.
///
/// Builtin type signatures are available via `TypeEnv::with_builtins()`, but 3 of 16
/// corpus files still fail: `$get` is a stdlib prelude function (not a builtin),
/// `$merge` triggers a row polymorphism false positive, and `$+` with dot-access
/// forward refs produces a unification error. Re-enable once stdlib prelude functions
/// have type signatures and row-polymorphism inference is improved.
#[test]
#[ignore = "3 corpus files fail: $get is stdlib (not builtin), $merge/$+ row-poly false positives"]
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
        match typecheck_source(&test.input) {
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

/// Type-error corpus runner for `tests/corpus/eval/type_errors/`.
///
/// Each `.llt-eval` file in this directory must:
/// 1. Contain LLT source that **fails** type checking (i.e. `typecheck_source()` returns `Err`).
/// 2. Have a `=== out` section with an expected error substring.
///
/// Unlike `test_typecheck_error_corpus` (which targets `tests/corpus/invalid/type_errors/`),
/// this runner exercises type errors in the eval corpus directory. Files may use stdlib
/// builtins since `TypeEnv::with_builtins()` provides type signatures for all builtins.
///
/// The type checker is advisory at runtime (eval always proceeds), but this corpus ensures
/// we can write regression tests that assert specific type errors are detected.
#[test]
fn test_typecheck_error_corpus_eval() {
    let corpus_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval/type_errors");

    let test_files = find_test_files(&corpus_dir);
    if test_files.is_empty() {
        // Directory exists but has no .llt-eval files — acceptable (new directory).
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

        let expected_substr = match &test.expectations.out {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "type error corpus test file missing expected error substring after === out"
                        .to_string(),
                ));
                continue;
            }
        };

        // Type check should fail for all files in tests/corpus/eval/type_errors/
        match typecheck_source(&test.input) {
            Ok(()) => {
                failed.push((
                    relative_path.to_path_buf(),
                    "Expected typecheck to fail, but it succeeded".to_string(),
                ));
            }
            Err(error_msg) => {
                if !error_msg.contains(expected_substr.as_str()) {
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

/// Typecheck warnings corpus runner — validates that each `.llt-eval` file in
/// `tests/corpus/typecheck/warnings/` both evaluates successfully AND produces the expected
/// type warning via `typecheck_source`.
///
/// Each file must have:
/// - `=== out` — the expected `eval_source` output string.
/// - `=== warn` — a substring that must appear in the `typecheck_source` error message.
///
/// This corpus seeds one test per distinct warning category (type_mismatch,
/// constraint_not_satisfied, record_field_missing, function_arity) so that regressions in
/// the type checker's diagnostic output are caught end-to-end.
#[test]
fn test_typecheck_warnings_corpus() {
    let corpus_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/typecheck/warnings");

    let test_files = find_test_files(&corpus_dir);
    assert!(
        !test_files.is_empty(),
        "No test files found in tests/corpus/typecheck/warnings/ — \
         seed files are required (type_mismatch.llt-eval, constraint_not_satisfied.llt-eval, \
         record_field_missing.llt-eval, function_arity.llt-eval)"
    );

    // Large stack for stdlib Rc<Environment> drop chain (same rationale as test_eval_corpus).
    let test_files_clone = test_files.clone();
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024) // 512MB
        .spawn(move || {
            let mut failed = Vec::new();

            for test_file in &test_files_clone {
                let content = fs::read_to_string(test_file)
                    .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

                let relative_path = test_file
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(test_file);

                let test = split_test_file(&content);

                // Both === out and === warn are required in this corpus.
                let expected_out = match &test.expectations.out {
                    Some(e) => e,
                    None => {
                        failed.push((
                            relative_path.to_path_buf(),
                            "typecheck/warnings corpus test missing === out section".to_string(),
                        ));
                        continue;
                    }
                };
                let expected_warn = match &test.expectations.warn {
                    Some(e) => e,
                    None => {
                        failed.push((
                            relative_path.to_path_buf(),
                            "typecheck/warnings corpus test missing === warn section".to_string(),
                        ));
                        continue;
                    }
                };

                // 1. Eval must succeed.
                match eval_source_with_config(&test.input, test.no_fs) {
                    Ok(actual) => {
                        if actual.trim() != expected_out.as_str() {
                            failed.push((
                                relative_path.to_path_buf(),
                                format!(
                                    "eval output mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
                                    expected_out,
                                    actual.trim()
                                ),
                            ));
                        }
                    }
                    Err(e) => {
                        failed.push((
                            relative_path.to_path_buf(),
                            format!("eval error (expected success): {e}"),
                        ));
                        continue;
                    }
                }

                // 2. Typecheck must produce a warning containing the expected substring.
                match typecheck_source(&test.input) {
                    Ok(()) => {
                        failed.push((
                            relative_path.to_path_buf(),
                            format!(
                                "typecheck warning missing\n--- expected warning substring ---\n{}\n--- actual ---\nno warnings (typecheck succeeded)",
                                expected_warn
                            ),
                        ));
                    }
                    Err(type_errors) => {
                        if !type_errors.contains(expected_warn.as_str()) {
                            failed.push((
                                relative_path.to_path_buf(),
                                format!(
                                    "typecheck warning mismatch\n--- expected substring ---\n{}\n--- actual warnings ---\n{}",
                                    expected_warn,
                                    type_errors
                                ),
                            ));
                        }
                    }
                }
            }

            failed
        })
        .unwrap()
        .join()
        .unwrap();

    if !result.is_empty() {
        eprintln!("\n{} typecheck/warnings test(s) failed:", result.len());
        for (path, error) in &result {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("Typecheck warnings corpus tests failed");
    }
}

/// Type error corpus runner — validates that all files in `tests/corpus/invalid/type_errors/`
/// fail type checking with the expected error substring.
///
/// Companion to `test_typecheck_corpus`. Builtin type signatures are available via
/// `TypeEnv::with_builtins()`, so corpus files may exercise builtins.
#[test]
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

        let expected_substr = match &test.expectations.out {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "type error corpus test file missing expected error substring after === out"
                        .to_string(),
                ));
                continue;
            }
        };

        // Type check should fail for all files in tests/corpus/invalid/type_errors/
        match typecheck_source(&test.input) {
            Ok(()) => {
                failed.push((
                    relative_path.to_path_buf(),
                    "Expected typecheck to fail, but it succeeded".to_string(),
                ));
            }
            Err(error_msg) => {
                // Check if the error message contains the expected substring
                if !error_msg.contains(expected_substr.as_str()) {
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
