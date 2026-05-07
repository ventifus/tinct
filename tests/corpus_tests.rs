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

/// Expected output sections from a test file.
#[derive(Debug)]
struct TestExpectations<'a> {
    /// Expected standard output (from `=== out` section).
    out: Option<&'a str>,
    /// Expected warnings (from `=== warn` section).
    warn: Option<&'a str>,
    /// Expected error substring (from `=== error` section).
    error: Option<&'a str>,
}

/// Parsed test file with optional directives.
struct TestFile<'a> {
    /// The LLT source code to evaluate (directives stripped).
    input: &'a str,
    /// Expected outputs for different channels.
    expectations: TestExpectations<'a>,
    /// Whether to enable `--no-fs` mode (from `# no_fs` directive).
    no_fs: bool,
}

/// Split a test file on labeled section delimiters (`=== out`, `=== warn`, `=== error`).
/// Uses `===` instead of `---` because `---` is a valid LLT document separator.
///
/// Supports directives on the first line:
/// - `# no_fs` — evaluate with filesystem access disabled (`no_fs: true`)
///
/// IMPORTANT: If the first line starts with `#`, it is treated as a directive line
/// and is STRIPPED from the input before evaluation. This means `#`-prefixed content
/// on line 1 is never evaluated, even if it's just a comment.
///
/// Expected output sections:
/// - `=== out` — expected standard output (AST Display for valid/, Value Debug for eval/)
/// - `=== warn` — expected type-warning substring; absent means assert zero type warnings
/// - `=== error` — expected error substring (must include [EXXX] error code)
///
/// A bare `===` (without a label) is a parse error — use `=== out` instead.
fn split_test_file(content: &str) -> TestFile {
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

    // Find all section delimiters
    let mut sections = Vec::new();
    let mut search_start = 0;

    while let Some(pos) = content[search_start..].find("\n===") {
        let abs_pos = search_start + pos;
        // Check what comes after "==="
        let after_delim = &content[abs_pos + 4..]; // skip "\n==="

        // Extract the label (text between === and the next newline)
        let label_end = after_delim.find('\n').unwrap_or(after_delim.len());
        let label = after_delim[..label_end].trim();

        sections.push((abs_pos, label));
        search_start = abs_pos + 4 + label_end;
    }

    // If no sections found, the entire content is input
    if sections.is_empty() {
        return TestFile {
            input: content,
            expectations: TestExpectations {
                out: None,
                warn: None,
                error: None,
            },
            no_fs,
        };
    }

    // First section starts at input, ends at first delimiter
    let input = &content[..sections[0].0 + 1]; // include trailing newline before ===

    // Parse sections
    let mut out = None;
    let mut warn = None;
    let mut error = None;

    for (i, (pos, label)) in sections.iter().enumerate() {
        // Content starts after "\n=== label\n"
        let label_line_start = pos + 4; // skip "\n==="
        let label_line_end = content[label_line_start..]
            .find('\n')
            .map(|p| label_line_start + p)
            .unwrap_or(content.len());
        let content_start = if label_line_end < content.len() {
            label_line_end + 1 // skip the newline after label
        } else {
            label_line_end
        };

        // Content ends at next section or EOF
        let content_end = sections
            .get(i + 1)
            .map(|(next_pos, _)| *next_pos + 1) // include trailing newline
            .unwrap_or(content.len());

        let section_content = &content[content_start..content_end];
        let trimmed = section_content.trim();

        match *label {
            "out" => {
                out = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            "warn" => {
                warn = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            "error" => {
                error = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            "" => {
                // Bare === without label
                panic!("bare '===' is no longer valid; use '=== out', '=== warn', or '=== error'");
            }
            other => {
                panic!(
                    "unknown section label '{}'; valid labels are 'out', 'warn', 'error'",
                    other
                );
            }
        }
    }

    TestFile {
        input,
        expectations: TestExpectations { out, warn, error },
        no_fs,
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

        // Use parse (full file) to verify the input is valid.
        // For expected output comparison, use parse_expression (single expr).
        match parse(test.input) {
            Ok(_) => {
                // Single-section files (no `=== out`) are parse-only tests: verifying that
                // the file parses without error is sufficient. The corpus README documents
                // this convention. Skip AST comparison when no expected section is present.
                let expected_output = match test.expectations.out {
                    Some(e) => e,
                    None => continue,
                };

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

        let expected_substr = match test.expectations.out {
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

        match parse(test.input) {
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

    // No valid/ file may contain a `=== warn` section.
    // The test_valid_corpus runner only calls parse() and never calls typecheck_source(),
    // so warn sections in valid/ files look like assertions but enforce nothing.
    // Typecheck tests belong in tests/corpus/eval/ where the runner checks warnings.
    let valid_dir = manifest_dir.join("tests/corpus/valid");
    let valid_files = find_test_files(&valid_dir);
    let mut warn_violations: Vec<String> = Vec::new();
    for test_file in &valid_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));
        if content.contains("\n=== warn") {
            let relative = test_file
                .strip_prefix(&manifest_dir)
                .unwrap_or(test_file)
                .display()
                .to_string();
            warn_violations.push(relative);
        }
    }
    if !warn_violations.is_empty() {
        eprintln!(
            "\n{} valid/ corpus file(s) contain === warn sections:",
            warn_violations.len()
        );
        for path in &warn_violations {
            eprintln!("  - {}", path);
        }
        panic!(
            "valid/ corpus files must not have === warn sections \
             (valid/ runner never calls typecheck_source; \
             put typecheck tests in eval/ instead)"
        );
    }
}

#[test]
fn test_eval_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/eval");
    let errors_dir = corpus_dir.join("errors");
    // type_errors/ files are expected to fail typecheck (not produce eval output);
    // they are handled by test_typecheck_error_corpus_eval instead.
    let type_errors_dir = corpus_dir.join("type_errors");

    let test_files: Vec<_> = find_test_files(&corpus_dir)
        .into_iter()
        .filter(|p| !p.starts_with(&errors_dir) && !p.starts_with(&type_errors_dir))
        .collect();
    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    // Spawn thread with large stack to prevent overflow in deeply-nested test cases.
    // Same rationale as test_eval_error_corpus: the stdlib Rc<Environment> recursive
    // drop at thread exit requires significant stack space (100+ MB in debug mode).
    // 512MB chosen to give comfortable headroom as the stdlib prelude grows over time.
    let test_files_clone = test_files.clone();
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024) // 512MB — debug-mode stdlib cleanup needs ~100MB; extra headroom for prelude growth
        .spawn(move || {
            let mut failed = Vec::new();

            for test_file in &test_files_clone {
                let content = fs::read_to_string(test_file)
                    .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

                let relative_path = test_file
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(test_file);

                let test = split_test_file(&content);

                // === out and === error are mutually exclusive (contradictory eval outcome).
                if test.expectations.out.is_some() && test.expectations.error.is_some() {
                    failed.push((
                        relative_path.to_path_buf(),
                        "test file has both === out and === error sections (contradictory: \
                         === out implies eval success, === error implies eval failure)"
                            .to_string(),
                    ));
                    continue;
                }

                // --- Channel 1: eval (=== out / === error) ---
                match eval_source(test.input) {
                    Ok(actual) => {
                        if let Some(expected_error) = test.expectations.error {
                            // Expected failure but eval succeeded.
                            failed.push((
                                relative_path.to_path_buf(),
                                format!(
                                    "expected eval failure (=== error), but eval succeeded\n\
                                     --- expected error substring ---\n{}\n\
                                     --- actual output ---\n{}",
                                    expected_error,
                                    actual.trim()
                                ),
                            ));
                        } else if let Some(expected_output) = test.expectations.out {
                            // === out present: assert exact output match.
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
                        // === out absent and === error absent: run-only test; no output assertion.
                    }
                    Err(e) => {
                        if let Some(expected_error) = test.expectations.error {
                            // === error present: check message contains expected substring and
                            // includes an [EXXX] error code.
                            let error_msg = format!("{}", e);
                            if !error_msg.contains(expected_error) {
                                failed.push((
                                    relative_path.to_path_buf(),
                                    format!(
                                        "eval error mismatch\n--- expected substring ---\n{}\n--- actual error ---\n{}",
                                        expected_error, error_msg
                                    ),
                                ));
                            }
                            if !has_error_code_prefix(&error_msg) {
                                failed.push((
                                    relative_path.to_path_buf(),
                                    format!(
                                        "actual error missing [EXXX] error code prefix\n--- actual error ---\n{}",
                                        error_msg
                                    ),
                                ));
                            }
                        } else {
                            // === error absent: eval must succeed.
                            failed.push((
                                relative_path.to_path_buf(),
                                format!("unexpected eval error (add === error section or fix the bug):\n{e}"),
                            ));
                        }
                    }
                }

                // --- Channel 2: typecheck (=== warn) — runs independently of eval outcome ---
                match test.expectations.warn {
                    Some(expected_warnings) => {
                        // === warn present: typecheck must produce warnings matching the substring.
                        match typecheck_source(test.input) {
                            Ok(()) => {
                                failed.push((
                                    relative_path.to_path_buf(),
                                    format!(
                                        "typecheck warning mismatch\n--- expected warnings ---\n{}\n--- actual ---\nno warnings (typecheck succeeded)",
                                        expected_warnings
                                    ),
                                ));
                            }
                            Err(type_errors) => {
                                if !type_errors.contains(expected_warnings) {
                                    failed.push((
                                        relative_path.to_path_buf(),
                                        format!(
                                            "typecheck warning mismatch\n--- expected warnings substring ---\n{}\n--- actual warnings ---\n{}",
                                            expected_warnings, type_errors
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                    None => {
                        // === warn absent: assert zero type warnings.
                        if let Err(type_errors) = typecheck_source(test.input) {
                            failed.push((
                                relative_path.to_path_buf(),
                                format!(
                                    "typecheck produced unexpected warnings (add === warn section or fix the warning):\n{}",
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
        eprintln!("\n{} eval test(s) failed:", result.len());
        for (path, error) in &result {
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

        let expected_substr = match test.expectations.out {
            Some(e) => e,
            None => {
                failed.push((
                    relative_path.to_path_buf(),
                    "eval error corpus test file missing expected error substring after === out"
                        .to_string(),
                ));
                continue;
            }
        };

        // Evaluate in a thread with a large stack. Two independent sources of
        // deep recursion require headroom:
        //
        // 1. typeassert_depth_exceeded_not_circular: each of the MAX_EVAL_DEPTH
        //    (256) LLT recursion levels maps to ~6-8 Rust frames inside
        //    materialize/eval/invoke_function.
        //
        // 2. Drop of the stdlib Rc<Environment> chain: create_stdlib_env()
        //    builds a letrec dict_env whose closures all hold Rc references
        //    back to that same env. When the thread's local bindings are
        //    dropped at function exit, the Rc refcount of dict_env reaches
        //    zero and Rust drops it recursively through the parent chain —
        //    one Rust frame per environment level. The stdlib prelude is large
        //    enough that this recursive drop exceeds 64 MB.
        //
        // 512 MB gives comfortable headroom above both limits.
        let input = test.input.to_string();
        let no_fs = test.no_fs;
        let eval_result = std::thread::Builder::new()
            .stack_size(512 * 1024 * 1024) // 512MB — debug-mode materialize() needs ~100MB at 256 levels; extra headroom for stdlib growth
            .spawn(move || eval_source_with_config(&input, no_fs))
            .unwrap()
            .join()
            .unwrap();

        // TODO(deferred): Span assertions in error corpus tests.
        //
        // Current state: error tests validate that eval fails and that the error message
        // contains the expected substring (which must include an [EXXX] error code prefix,
        // enforced by test_eval_error_corpus_has_error_codes). This catches most regressions.
        //
        // What would be needed for span assertions:
        // 1. Extend the test file format to carry span expectations, e.g.:
        //      [call $+ 1]
        //      === out
        //      [E005] arity mismatch
        //      SPAN: 1:1-1:13
        //
        // 2. Parse the SPAN: directive in split_test_file(), storing it in TestFile.
        //    The span format would need a stable text representation (line:col-line:col).
        //
        // 3. After catching the Err, extract definition_span / materialization_span from
        //    EvalError (already public fields) and format them as "line:col-line:col" using
        //    the source text byte offsets + line-number index.
        //
        // 4. Compare the formatted spans against the expected SPAN: value.
        //
        // Why deferred: The primary value of span testing is regression protection — catching
        // when a parser or evaluator change accidentally moves an error pointer off by one.
        // The current substring match on error codes already catches message regressions.
        // Span testing adds significant infrastructure complexity (text → line/col mapping,
        // test file format extension, brittle to whitespace-only formatting changes in tests)
        // for moderate incremental benefit. The right time to implement this is when a
        // span regression is actually caught in review and needs a reproducible test case.
        //
        // Tracked in TODO.md under test-infra.
        match eval_result {
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

                // Validate that the actual runtime error includes an [EXXX] error code.
                // Error codes are part of the public API (visible in all error display output,
                // documented in doc/10-errors.md §9.2). If the error code is missing from
                // the actual error, the ErrorKind::code() implementation has regressed.
                if !has_error_code_prefix(&error_msg) {
                    failed.push((
                        relative_path.to_path_buf(),
                        format!(
                            "Actual error message missing [EXXX] error code prefix\n--- actual error ---\n{}",
                            error_msg
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

        // Same 512 MB rationale as test_eval_error_corpus above:
        // handles both MAX_EVAL_DEPTH recursive frames and the stdlib
        // Rc<Environment> recursive drop at thread exit.
        let input = test.input.to_string();
        let no_fs = test.no_fs;
        let eval_result = std::thread::Builder::new()
            .stack_size(512 * 1024 * 1024) // 512MB — debug-mode materialize() needs ~100MB at 256 levels; extra headroom for stdlib growth
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

        let expected_substr = match test.expectations.out {
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
        match typecheck_source(test.input) {
            Ok(()) => {
                failed.push((
                    relative_path.to_path_buf(),
                    "Expected typecheck to fail, but it succeeded".to_string(),
                ));
            }
            Err(error_msg) => {
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
                let expected_out = match test.expectations.out {
                    Some(e) => e,
                    None => {
                        failed.push((
                            relative_path.to_path_buf(),
                            "typecheck/warnings corpus test missing === out section".to_string(),
                        ));
                        continue;
                    }
                };
                let expected_warn = match test.expectations.warn {
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
                match eval_source(test.input) {
                    Ok(actual) => {
                        if actual.trim() != expected_out {
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
                match typecheck_source(test.input) {
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
                        if !type_errors.contains(expected_warn) {
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

        let expected_substr = match test.expectations.out {
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
    let content = "# no_fs\n[call $include \"file.llt\"]\n=== out\nfilesystem access is disabled";
    let test = split_test_file(content);
    assert_eq!(test.input, "[call $include \"file.llt\"]\n");
    assert_eq!(test.expectations.out, Some("filesystem access is disabled"));
    assert!(test.no_fs, "no_fs directive should be detected");
}

#[test]
fn test_split_test_file_no_fs_substring_false_positive() {
    let content = "# testing no_fs filesystem semantics\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out, Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for substring match 'no_fs'"
    );
}

#[test]
fn test_split_test_file_no_fs_prefix_false_positive() {
    let content = "# no_fs_path\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out, Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for token 'no_fs_path'"
    );
}

#[test]
fn test_split_test_file_no_directive() {
    let content = "[x: 1 y: 2]\n=== out\n[\"x\": 1  \"y\": 2]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1 y: 2]\n");
    assert_eq!(test.expectations.out, Some("[\"x\": 1  \"y\": 2]"));
    assert!(!test.no_fs, "no_fs should default to false");
}

#[test]
fn test_split_test_file_eof_without_trailing_newline() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out, Some("[\"x\": 1]"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_missing_delimiter() {
    let content = "[x: 1]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]");
    assert_eq!(test.expectations.out, None);
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_delimiter_in_expected() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]  # comment with === in it";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(
        test.expectations.out,
        Some("[\"x\": 1]  # comment with === in it")
    );
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_error_section() {
    let content = "[call $error \"boom\"]\n=== error\n[E024]";
    let test = split_test_file(content);
    assert_eq!(test.input, "[call $error \"boom\"]\n");
    assert_eq!(test.expectations.error, Some("[E024]"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_warn_section() {
    let content = "[x: 1]\n=== warn\ndeprecated feature used";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.warn, Some("deprecated feature used"));
}

#[test]
fn test_split_test_file_empty_content() {
    let content = "";
    let test = split_test_file(content);
    assert_eq!(test.input, "");
    assert_eq!(test.expectations.out, None);
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_multiple_sections() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]\n=== warn\ndeprecated\n=== error\nshould not reach";
    let test = split_test_file(content);
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out, Some("[\"x\": 1]"));
    assert_eq!(test.expectations.warn, Some("deprecated"));
    assert_eq!(test.expectations.error, Some("should not reach"));
}

#[test]
#[should_panic(expected = "bare '===' is no longer valid")]
fn test_split_test_file_bare_delimiter_panics() {
    let content = "[x: 1]\n===\n[\"x\": 1]";
    let _ = split_test_file(content);
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

/// Check that a corpus test file's expected section (after `===`) does not contain
/// a bare `---` line on its own.
///
/// A bare `---` line in the **expected** section is almost certainly a mistake:
/// `---` is the LLT document separator and belongs in the **input** section.
/// If an author writes:
///
///   [x: 1]
///   ---
///   %.x
///   === out
///   1
///   ---           ← THIS IS THE BUG: should be in the input section
///   some_more
///
/// the `---` ends up in the expected output string, causing an "expected substring mismatch"
/// that's hard to diagnose. This validator catches that mistake explicitly.
///
/// Note: `---` is legitimate in the input section (it is valid LLT syntax) and is
/// allowed there. Only a bare `---` in the EXPECTED section is flagged.
fn check_no_llt_separator_in_expected(content: &str, path: &std::path::Path) -> Option<String> {
    // Find the first labeled section delimiter (=== out / === warn / === error).
    // All corpus files now use the labeled format; bare `===` is a parse error.
    if let Some(pos) = content.find("\n=== ") {
        // Skip past the label line (e.g. "=== out\n") to reach the section content.
        let after_label = &content[pos + 5..];
        let label_end = after_label
            .find('\n')
            .map(|p| p + 1)
            .unwrap_or(after_label.len());
        let expected_section = &after_label[label_end..];
        // Check each line in the expected section for a bare "---"
        for line in expected_section.lines() {
            if line == "---" {
                return Some(format!(
                    "{}: bare `---` on its own line found in expected section (after the first `===` section). \
                     `---` is valid LLT document separator syntax and belongs in the input \
                     section (before the first `===` section). If this `---` is intentional expected output, \
                     it cannot be on its own line — it must be part of a longer expected string.",
                    path.display()
                ));
            }
        }
    }
    None
}

/// Validates that no corpus test file has a bare `---` line in its expected section.
/// Run as a separate test so failures are clearly attributed.
#[test]
fn test_no_llt_separator_in_expected_section() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let corpus_dir = manifest_dir.join("tests/corpus");

    let all_test_files = find_test_files(&corpus_dir);
    let mut violations = Vec::new();

    for test_file in &all_test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        if let Some(msg) = check_no_llt_separator_in_expected(&content, test_file) {
            violations.push(msg);
        }
    }

    if !violations.is_empty() {
        eprintln!(
            "\n{} corpus test(s) have bare `---` in expected section:",
            violations.len()
        );
        for msg in &violations {
            eprintln!("  - {}", msg);
        }
        panic!(
            "Corpus test files must not use `---` on its own line in the expected section. \
             `---` is a valid LLT document separator and belongs only in the input section (before `===`)."
        );
    }
}

/// Unit tests for check_no_llt_separator_in_expected().
#[test]
fn test_check_no_llt_separator_dash_in_input_allowed() {
    // `---` in the INPUT section (before `=== out`) is valid LLT — allowed.
    let content = "[x: 1]\n---\n%.x\n=== out\n1\n";
    let path = std::path::Path::new("dummy.llt-eval");
    assert!(
        check_no_llt_separator_in_expected(content, path).is_none(),
        "`---` in the input section should be allowed"
    );
}

#[test]
fn test_check_no_llt_separator_dash_in_expected_flagged() {
    // `---` in the EXPECTED section (after `=== out`) is almost certainly a mistake.
    let content = "[x: 1]\n=== out\n1\n---\nmore stuff\n";
    let path = std::path::Path::new("dummy.llt-eval");
    let result = check_no_llt_separator_in_expected(content, path);
    assert!(
        result.is_some(),
        "`---` in the expected section should be flagged"
    );
    assert!(
        result.unwrap().contains("bare `---`"),
        "Error message should mention bare `---`"
    );
}

#[test]
fn test_check_no_llt_separator_no_delim_skipped() {
    // Files without `=== ` labeled sections have no expected section — nothing to flag.
    let content = "[x: 1]\n---\n%.x\n";
    let path = std::path::Path::new("dummy.llt-eval");
    assert!(
        check_no_llt_separator_in_expected(content, path).is_none(),
        "Files without labeled section delimiter have no expected section"
    );
}

#[test]
fn test_check_no_llt_separator_partial_dash_allowed() {
    // `----` or `--- more text` in the expected section is NOT a bare `---` — allowed.
    let content = "[x: 1]\n=== out\nexpected output\n---- not a separator\n";
    let path = std::path::Path::new("dummy.llt-eval");
    assert!(
        check_no_llt_separator_in_expected(content, path).is_none(),
        "`---- more` is not a bare `---` separator — should be allowed"
    );
}

#[test]
fn test_split_test_file_unknown_label_panics() {
    let content = "[x: 1]\n=== unknown\nsome content";
    let result = std::panic::catch_unwind(|| split_test_file(content));
    assert!(result.is_err(), "Unknown section label should panic");
}
