// Test infrastructure uses std::fs for corpus file reading — no cap_std available in test harness.
// Clippy lint suppression: disallowed_methods, useless_format, manual_ok_err are benign in test code.
#![allow(
    clippy::disallowed_methods,
    clippy::useless_format,
    clippy::manual_ok_err,
    clippy::to_string_in_format_args,
    clippy::approx_constant,
    clippy::useless_conversion
)]

mod test_helpers;

use std::fs;
use std::path::PathBuf;
use test_helpers::{find_test_files, run_corpus_dir, split_test_file, CorpusOutcome};
use tinct::{
    eval_source_with_cap_net, eval_source_with_config, parse, parse_surface_expression,
    typecheck_source, typecheck_source_errors_only,
};

#[test]
fn test_valid_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/valid");

    // Spawn thread with large stack to match the RUST_MIN_STACK used in the justfile
    // for lib tests (64MB). The default 2MB thread stack may be insufficient for
    // deeply-nested parse/typecheck calls when running without RUST_MIN_STACK set.
    let result = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024) // 64MB — matches RUST_MIN_STACK in justfile
        .spawn(move || {
            run_corpus_dir(&corpus_dir, &[], |test| {
                // Parse-only pipeline: parse() + typecheck_source()
                // Parse failure maps to error, success maps to output (for AST comparison)
                match parse(&test.input) {
                    Ok(_) => {
                        // For files with === out, we need to produce the AST Display string
                        let output = if test.expectations.out.is_some() {
                            match parse_surface_expression(&test.input) {
                                Ok(surface_node) => Some(format!("{}", surface_node)),
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

                        // Run typecheck to get warnings (errors only, not quality diagnostics)
                        let warnings = typecheck_source_errors_only(&test.input).err();

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
            })
        })
        .unwrap()
        .join()
        .unwrap();

    if !result.is_empty() {
        eprintln!("\n{} valid test(s) failed:", result.len());
        for failure in &result {
            eprintln!("  - {}: {}", failure.path.display(), failure.message);
        }
        panic!("Valid corpus tests failed");
    }
}

#[test]
// CORPUS-OK: test infrastructure reads corpus files via std::fs — no cap_std available here
#[allow(clippy::disallowed_methods)]
fn test_invalid_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/invalid");
    // type_errors/ files are expected to fail typecheck (not parse);
    // they are handled by test_typecheck_error_corpus instead.
    let type_errors_dir = corpus_dir.join("type_errors");

    let test_files: Vec<_> = find_test_files(&corpus_dir)
        .into_iter()
        .filter(|p| !p.starts_with(&type_errors_dir))
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

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("test file format error: {}", e),
                ));
                continue;
            }
        };

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

        // With error recovery, parse() returns Ok(ParseOutput) even when there are errors.
        // Check that the input produces parse errors (either fatal or recovered).
        match parse(&test.input) {
            Ok(output) => {
                // Parse succeeded — check for recovered errors in output.errors
                if output.errors.is_empty() {
                    failed.push((
                        relative_path.to_path_buf(),
                        "Expected parse to produce errors, but got successful parse with no errors"
                            .to_string(),
                    ));
                } else {
                    // Recovered errors present — check if any match the expected substring
                    let all_errors = output
                        .errors
                        .iter()
                        .map(|e| format!("{}", e))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !all_errors.contains(expected_substr.as_str()) {
                        failed.push((
                            relative_path.to_path_buf(),
                            format!(
                                "Error message mismatch\n--- expected substring ---\n{}\n--- actual recovered errors ---\n{}",
                                expected_substr, all_errors
                            ),
                        ));
                    }
                }
            }
            Err(e) => {
                // Fatal parse error (lexer failure, unclosed brackets, etc.)
                let error_msg = format!("{}", e);
                if !error_msg.contains(expected_substr.as_str()) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Error message mismatch\n--- expected substring ---\n{}\n--- actual fatal error ---\n{}",
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
        "tests/corpus/eval/builtins/errors",
        "tests/corpus/eval/cross_feature",
        "tests/corpus/eval/errors",
        "tests/corpus/eval/functions",
        "tests/corpus/eval/laziness",
        "tests/corpus/eval/regressions",
        "tests/corpus/eval/stdlib",
        "tests/corpus/eval/stdlib/codecs",
        "tests/corpus/eval/stdlib/protocols",
        "tests/corpus/eval/macros",
        "tests/corpus/eval/type_assertions",
        "tests/corpus/eval/type_errors",
        "tests/corpus/eval/typecheck",
        "tests/corpus/eval/pipeline",
        "tests/corpus/eval/type_system",
        "tests/corpus/eval/letrec",
        "tests/corpus/eval/underscore",
        "tests/corpus/eval/documents",
        "tests/corpus/eval/slow",
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
    const EVAL_LAZINESS_MIN: usize = 40;
    const EVAL_BUILTINS_MIN: usize = 120;
    const EVAL_STDLIB_MIN: usize = 185;
    const EVAL_ERRORS_MIN: usize = 117;
    const EVAL_MACROS_MIN: usize = 40;
    const TYPECHECK_WARNINGS_MIN: usize = 14;

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

    let macros_count = find_test_files(&manifest_dir.join("tests/corpus/eval/macros")).len();
    assert!(
        macros_count >= EVAL_MACROS_MIN,
        "tests/corpus/eval/macros/ has {} tests, expected at least {}",
        macros_count,
        EVAL_MACROS_MIN
    );

    let typecheck_warnings_count =
        find_test_files(&manifest_dir.join("tests/corpus/typecheck/warnings")).len();
    assert!(
        typecheck_warnings_count >= TYPECHECK_WARNINGS_MIN,
        "tests/corpus/typecheck/warnings/ has {} tests, expected at least {}",
        typecheck_warnings_count,
        TYPECHECK_WARNINGS_MIN
    );
}

#[test]
fn test_eval_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval");
    // type_errors/ files are expected to fail typecheck (not produce eval output);
    // they are handled by test_typecheck_error_corpus_eval instead.
    let type_errors_dir = corpus_dir.join("type_errors");
    // slow/ contains 10,000-iteration TCO tests excluded from the default CI run;
    // use `just test-slow` to run them explicitly.
    let slow_dir = corpus_dir.join("slow");

    // Spawn thread with large stack to prevent overflow in deeply-nested test cases.
    // Same rationale as test_eval_error_corpus: the stdlib Rc<Environment> recursive
    // drop at thread exit requires significant stack space (100+ MB in debug mode).
    // 512MB chosen to give comfortable headroom as the stdlib prelude grows over time.
    let result = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024) // 256MB — debug-mode stdlib cleanup needs ~100MB; extra headroom for prelude growth
        .spawn(move || {
            run_corpus_dir(
                &corpus_dir,
                &[type_errors_dir.as_path(), slow_dir.as_path()],
                |test| {
                    // Clear the stdlib cache before each test to prevent memory accumulation.
                    // The STDLIB_ARENA_CACHE retains Arc<Thunk> references from previous test
                    // iterations, preventing the ThunkArena from being freed. After ~500 iterations
                    // in an 8GB container, cumulative memory growth causes OOM/SIGKILL.
                    tinct::clear_stdlib_cache();
                    // Return freed heap pages to the OS. Without this, glibc malloc retains
                    // freed pages in thread-local pools, causing RSS to grow across tests even
                    // though each test's env+arena is freed. The 512MB stack thread + accumulated
                    // malloc pools can exceed the 10GB container limit.
                    #[cfg(target_os = "linux")]
                    unsafe {
                        libc::malloc_trim(0);
                    }

                    // Eval pipeline: eval_source_with_config() + typecheck_source()
                    let eval_result = if test.cap_net.is_empty() {
                        eval_source_with_config(&test.input, test.no_fs)
                    } else {
                        eval_source_with_cap_net(&test.input, test.no_fs, &test.cap_net)
                    };
                    let typecheck_result = typecheck_source_errors_only(&test.input);

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
                },
            )
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

/// Slow eval corpus runner — runs `tests/corpus/eval/slow/` in isolation.
///
/// These tests are excluded from the default `just test` / `just ci` run because they
/// execute 10,000-iteration loops to verify TCO. Run them with `just test-slow` when
/// you want to confirm that tail-call optimization handles large iteration counts.
#[test]
#[ignore = "slow: 10,000-iteration TCO tests; run with `just test-slow`"]
fn test_eval_corpus_slow() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval/slow");

    let result = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            run_corpus_dir(&corpus_dir, &[], |test| {
                tinct::clear_stdlib_cache();
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::malloc_trim(0);
                }

                let eval_result = if test.cap_net.is_empty() {
                    eval_source_with_config(&test.input, test.no_fs)
                } else {
                    eval_source_with_cap_net(&test.input, test.no_fs, &test.cap_net)
                };
                let typecheck_result = typecheck_source_errors_only(&test.input);

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
        eprintln!("\n{} slow eval test(s) failed:", result.len());
        for failure in &result {
            eprintln!("  - {}: {}", failure.path.display(), failure.message);
        }
        panic!("Slow eval corpus tests failed");
    }
}

/// Typecheck corpus runner — validates that all `.llt-eval` files in
/// `tests/corpus/eval/typecheck/` pass type checking without errors.
///
/// All stdlib builtins and prelude functions have type signatures via `imports::build_prelude_env()`.
#[test]
fn test_typecheck_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval/typecheck");
    // Exclude warnings/ — those are owned by test_typecheck_warnings_corpus and are
    // explicitly intended to produce warnings (not held to the zero-errors standard).
    let warnings_dir = corpus_dir.join("warnings");

    let test_files = find_test_files(&corpus_dir);
    if test_files.is_empty() {
        return;
    }

    let mut failed = Vec::new();

    for test_file in &test_files {
        if test_file.starts_with(&warnings_dir) {
            continue;
        }
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("test file format error: {}", e),
                ));
                continue;
            }
        };

        // Type check should succeed for all files in tests/corpus/eval/typecheck/
        // Use errors-only variant: quality diagnostics (e.g. "inferred Unknown") are
        // advisory and expected for polymorphic/open-record patterns in this corpus.
        match typecheck_source_errors_only(&test.input) {
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
/// builtins since `build_builtins_type_env()` provides type signatures for all builtins.
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

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("test file format error: {}", e),
                ));
                continue;
            }
        };

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

                let test = match split_test_file(&content) {
                    Ok(t) => t,
                    Err(e) => {
                        failed.push((
                            relative_path.to_path_buf(),
                            format!("test file format error: {}", e),
                        ));
                        continue;
                    }
                };

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

                // Clear the stdlib cache before each test to prevent memory accumulation.
                tinct::clear_stdlib_cache();

                // 1. Eval must succeed.
                let eval_result_tc = if test.cap_net.is_empty() {
                    eval_source_with_config(&test.input, test.no_fs)
                } else {
                    eval_source_with_cap_net(&test.input, test.no_fs, &test.cap_net)
                };
                match eval_result_tc {
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
/// `build_builtins_type_env()`, so corpus files may exercise builtins.
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

        let test = match split_test_file(&content) {
            Ok(t) => t,
            Err(e) => {
                failed.push((
                    relative_path.to_path_buf(),
                    format!("test file format error: {}", e),
                ));
                continue;
            }
        };

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
