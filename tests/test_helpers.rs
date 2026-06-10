//! Shared test infrastructure for corpus test runners.
//!
//! This module provides:
//! - `split_test_file()` — parse test files with labeled sections (`=== out`, `=== warn`, `=== error`)
//! - `run_corpus_dir()` — unified corpus test runner with channel validation and error code checks

#![allow(dead_code)]
// Functions used by different test crates
// Test infrastructure uses std::fs for corpus file reading — no cap_std available in test harness.
#![allow(
    clippy::disallowed_methods,
    clippy::useless_format,
    clippy::approx_constant,
    clippy::doc_lazy_continuation
)]

use std::fs;
use std::path::{Path, PathBuf};

/// Expected output sections from a test file.
#[derive(Debug)]
pub struct TestExpectations {
    /// Expected standard output (from `=== out` section).
    pub out: Option<String>,
    /// Expected warnings (from `=== warn` section).
    pub warn: Option<String>,
    /// Expected error substring (from `=== error` section).
    pub error: Option<String>,
    /// Expected info/log output (from `=== info` section).
    pub info: Option<String>,
}

/// Parsed test file with optional directives.
#[derive(Debug)]
pub struct TestFile {
    /// The LLT source code to evaluate (directives stripped).
    pub input: String,
    /// Expected outputs for different channels.
    pub expectations: TestExpectations,
    /// Whether to enable `--no-fs` mode (from `# no_fs` directive).
    pub no_fs: bool,
    /// NetCap entries to inject, parsed from `# cap_net NAME=ENTRY` tokens.
    /// Each entry is `(cap_name, allowlist_entry_string)` e.g. `("nc", "*.local")`.
    pub cap_net: Vec<(String, String)>,
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
/// A bare `===` (without a label) returns an error — use `=== out` instead.
pub fn split_test_file(content: &str) -> Result<TestFile, String> {
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

    // Parse space-separated directive tokens after the leading `#`.
    // Uses a stateful parser — only recognized sequences are processed:
    //   no_fs           — evaluate with filesystem access disabled
    //   cap_net NAME=ENTRY [NAME=ENTRY ...]  — inject NetCap entries
    //
    // If the line contains any unrecognized token (i.e. it's a plain comment),
    // all directive parsing is abandoned and defaults are used. This prevents
    // false positives on comment lines like `# x >= y: true`.
    let mut no_fs = false;
    let mut cap_net: Vec<(String, String)> = Vec::new();
    let mut parse_ok = true;

    if let Some(tokens_str) = directives_line.strip_prefix('#') {
        let mut in_cap_net = false;
        for token in tokens_str.split_whitespace() {
            if token == "no_fs" {
                no_fs = true;
                in_cap_net = false;
            } else if token == "cap_net" {
                in_cap_net = true;
            } else if in_cap_net {
                // Consume NAME=ENTRY tokens after cap_net keyword
                if let Some((name, entry)) = token.split_once('=') {
                    if !name.is_empty() && !entry.is_empty() {
                        cap_net.push((name.to_string(), entry.to_string()));
                    } else {
                        parse_ok = false;
                        break;
                    }
                } else {
                    // Non-NAME=ENTRY token after cap_net — not a directive line
                    parse_ok = false;
                    break;
                }
            } else {
                // Unrecognized token in initial state — not a directive line
                parse_ok = false;
                break;
            }
        }
    }

    if !parse_ok {
        no_fs = false;
        cap_net.clear();
    }

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
        return Ok(TestFile {
            input: content.to_string(),
            expectations: TestExpectations {
                out: None,
                warn: None,
                error: None,
                info: None,
            },
            no_fs,
            cap_net,
        });
    }

    // First section starts at input, ends at first delimiter
    let input = &content[..sections[0].0 + 1]; // include trailing newline before ===

    // Parse sections
    let mut out = None;
    let mut warn = None;
    let mut error = None;
    let mut info = None;

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
                    Some(trimmed.to_string())
                };
            }
            "warn" => {
                warn = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "error" => {
                error = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "info" => {
                info = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "" => {
                // Bare === without label
                return Err(
                    "bare '===' is no longer valid; use '=== out', '=== warn', '=== error', or '=== info'"
                        .to_string(),
                );
            }
            other => {
                return Err(format!(
                    "unknown section label '{}'; valid labels are 'out', 'warn', 'error', 'info'",
                    other
                ));
            }
        }
    }

    Ok(TestFile {
        input: input.to_string(),
        expectations: TestExpectations {
            out,
            warn,
            error,
            info,
        },
        no_fs,
        cap_net,
    })
}

/// Outcome from running a corpus test through a pipeline.
pub struct CorpusOutcome {
    /// Standard output (eval result or AST display).
    pub output: Option<String>,
    /// Type warnings (from typecheck).
    pub warnings: Option<String>,
    /// Error message (from eval or parse failure).
    pub error: Option<String>,
}

/// A failed test case with its path and failure message.
pub struct Failure {
    pub path: PathBuf,
    pub message: String,
}

/// Recursively find all .llt-eval files in a directory
// CORPUS-OK: test infrastructure reads corpus dir via std::fs — no cap_std available here
#[allow(clippy::disallowed_methods)]
pub fn find_test_files(dir: &Path) -> Vec<PathBuf> {
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

/// Check if error message contains an error code pattern like [E001], [E099], [T000], etc.
///
/// IMPORTANT: This function matches exactly 3 digits ([E\d\d\d] or [T\d\d\d]).
/// LLT error codes use the 3-digit format:
/// - E001-E999 for eval/runtime errors
/// - T000-T999 for type checker errors
///
/// If the error code format changes, update this function.
fn has_error_code_prefix(error_msg: &str) -> bool {
    // Look for pattern [EXXX] or [TXXX] where XXX are exactly three digits
    error_msg.chars().collect::<Vec<_>>().windows(6).any(|w| {
        w[0] == '['
            && (w[1] == 'E' || w[1] == 'T')
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[4].is_ascii_digit()
            && w[5] == ']'
    })
}

/// Unified corpus test runner.
///
/// Finds all `.llt-eval` files in `dir` (excluding `excludes`), parses them with
/// `split_test_file()`, applies guards (mutual exclusivity of `=== out` + `=== error`,
/// non-empty sections), calls the provided `pipeline` closure, compares each channel
/// against expectations, and returns a list of failures.
///
/// Guards enforced:
/// - `=== out` and `=== error` are mutually exclusive (contradictory eval outcome)
/// - `=== error` section must be non-empty (blank error section is authoring error)
/// - `=== warn` section must be non-empty (blank warn section is authoring error)
/// - When `CorpusOutcome.error` is `Some`, it must contain an `[EXXX]` error code
///
/// The `pipeline` closure is called with each `TestFile` and returns a `CorpusOutcome`.
/// Comparison logic:
/// - `outcome.output` vs `expectations.out` — exact match (trimmed)
/// - `outcome.warnings` vs `expectations.warn` — substring match
/// - `outcome.error` vs `expectations.error` — substring match + error code check
// CORPUS-OK: test infrastructure reads corpus files via std::fs — no cap_std available here
#[allow(clippy::disallowed_methods)]
pub fn run_corpus_dir(
    dir: &Path,
    excludes: &[&Path],
    pipeline: impl Fn(&TestFile) -> CorpusOutcome,
) -> Vec<Failure> {
    let test_files: Vec<_> = find_test_files(dir)
        .into_iter()
        .filter(|p| !excludes.iter().any(|excl| p.starts_with(excl)))
        .collect();

    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        dir.display()
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
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: format!("test file format error: {}", e),
                });
                continue;
            }
        };

        // Guard: === out and === error are mutually exclusive
        if test.expectations.out.is_some() && test.expectations.error.is_some() {
            failed.push(Failure {
                path: relative_path.to_path_buf(),
                message: "test file has both === out and === error sections (contradictory: \
                          === out implies eval success, === error implies eval failure)"
                    .to_string(),
            });
            continue;
        }

        // Guard: === error section must be non-empty
        if let Some(ref err_text) = test.expectations.error {
            if err_text.is_empty() {
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: "=== error section is empty (blank error section is authoring error)"
                        .to_string(),
                });
                continue;
            }
        }

        // Guard: === warn section must be non-empty
        if let Some(ref warn_text) = test.expectations.warn {
            if warn_text.is_empty() {
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: "=== warn section is empty (blank warn section is authoring error)"
                        .to_string(),
                });
                continue;
            }
        }

        // Guard: === error section must contain [EXXX] error code prefix
        if let Some(ref err_text) = test.expectations.error {
            if !has_error_code_prefix(err_text) {
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: format!(
                        "error test missing [EXXX] code prefix in === error section\n\
                         --- current error text ---\n{}\n\
                         All error tests must include the error code (e.g., [E001], [E020], etc.)",
                        err_text
                    ),
                });
                continue;
            }
        }

        // Run the pipeline
        let outcome = pipeline(&test);

        // Channel 1: output (=== out / === error)
        match (&outcome.output, &outcome.error) {
            (Some(actual_output), None) => {
                // Eval succeeded
                if let Some(expected_error) = &test.expectations.error {
                    failed.push(Failure {
                        path: relative_path.to_path_buf(),
                        message: format!(
                            "expected eval failure (=== error), but eval succeeded\n\
                             --- expected error substring ---\n{}\n\
                             --- actual output ---\n{}",
                            expected_error,
                            actual_output.trim()
                        ),
                    });
                } else if let Some(expected_output) = &test.expectations.out {
                    // === out present: assert exact output match
                    if actual_output.trim() != expected_output {
                        failed.push(Failure {
                            path: relative_path.to_path_buf(),
                            message: format!(
                                "eval output mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
                                expected_output,
                                actual_output.trim()
                            ),
                        });
                    }
                }
                // === out absent and === error absent: run-only test; no output assertion
            }
            (None, Some(actual_error)) => {
                // Eval failed
                if let Some(expected_error) = &test.expectations.error {
                    // === error present: check message contains expected substring
                    if !actual_error.contains(expected_error) {
                        failed.push(Failure {
                            path: relative_path.to_path_buf(),
                            message: format!(
                                "eval error mismatch\n--- expected substring ---\n{}\n--- actual error ---\n{}",
                                expected_error, actual_error
                            ),
                        });
                    }
                    // Check for [EXXX] error code
                    if !has_error_code_prefix(actual_error) {
                        failed.push(Failure {
                            path: relative_path.to_path_buf(),
                            message: format!(
                                "actual error missing [EXXX] error code prefix\n--- actual error ---\n{}",
                                actual_error
                            ),
                        });
                    }
                } else {
                    // === error absent: eval must succeed
                    failed.push(Failure {
                        path: relative_path.to_path_buf(),
                        message: format!(
                            "unexpected eval error (add === error section or fix the bug):\n{}",
                            actual_error
                        ),
                    });
                }
            }
            (Some(_), Some(_)) => {
                // Both output and error — pipeline bug
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: "pipeline returned both output and error (pipeline bug)".to_string(),
                });
            }
            (None, None) => {
                // Neither output nor error — pipeline bug (unless expectations are also None)
                if test.expectations.out.is_some() || test.expectations.error.is_some() {
                    failed.push(Failure {
                        path: relative_path.to_path_buf(),
                        message: "pipeline returned neither output nor error (pipeline bug)"
                            .to_string(),
                    });
                }
            }
        }

        // Channel 2: warnings (=== warn) — runs independently of eval outcome
        // Path-based stripping: mirror update_corpus behavior. Warnings are only
        // meaningful when the test is in a warn/ directory; elsewhere they are stripped
        // so tests don't fail just because the type checker is advisory.
        let path_str = test_file.to_string_lossy();
        let actual_warnings = if path_str.contains("/warnings/") || path_str.contains("/warn/") {
            outcome.warnings.as_deref()
        } else {
            None
        };
        match (&actual_warnings, &test.expectations.warn) {
            (Some(actual_warnings), Some(expected_warnings)) => {
                // === warn present: typecheck must produce warnings matching the substring
                if !actual_warnings.contains(expected_warnings) {
                    failed.push(Failure {
                        path: relative_path.to_path_buf(),
                        message: format!(
                            "typecheck warning mismatch\n--- expected substring ---\n{}\n--- actual warnings ---\n{}",
                            expected_warnings, actual_warnings
                        ),
                    });
                }
            }
            (None, Some(expected_warnings)) => {
                // Expected warnings but got none
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: format!(
                        "typecheck warning mismatch\n--- expected warnings ---\n{}\n--- actual ---\nno warnings (typecheck succeeded)",
                        expected_warnings
                    ),
                });
            }
            (Some(actual_warnings), None) => {
                // === warn absent: assert zero type warnings
                failed.push(Failure {
                    path: relative_path.to_path_buf(),
                    message: format!(
                        "typecheck produced unexpected warnings (fix the warning or move this test to a warnings/ directory):\n{}",
                        actual_warnings
                    ),
                });
            }
            (None, None) => {
                // No warnings expected, none produced — OK
            }
        }
    }

    failed
}

// ---------------------------------------------------------------------------
// Unit tests for split_test_file()
// ---------------------------------------------------------------------------

#[test]
fn test_split_test_file_no_fs_directive() {
    let content = "# no_fs\n[call $include \"file.llt\"]\n=== out\nfilesystem access is disabled";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[call $include \"file.llt\"]\n");
    assert_eq!(
        test.expectations.out.as_deref(),
        Some("filesystem access is disabled")
    );
    assert!(test.no_fs, "no_fs directive should be detected");
}

#[test]
fn test_split_test_file_no_fs_substring_false_positive() {
    let content = "# testing no_fs filesystem semantics\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for substring match 'no_fs'"
    );
}

#[test]
fn test_split_test_file_no_fs_prefix_false_positive() {
    let content = "# no_fs_path\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert!(
        !test.no_fs,
        "no_fs should NOT be set for token 'no_fs_path'"
    );
}

#[test]
fn test_split_test_file_no_directive() {
    let content = "[x: 1 y: 2]\n=== out\n[\"x\": 1  \"y\": 2]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1 y: 2]\n");
    assert_eq!(
        test.expectations.out.as_deref(),
        Some("[\"x\": 1  \"y\": 2]")
    );
    assert!(!test.no_fs, "no_fs should default to false");
}

#[test]
fn test_split_test_file_eof_without_trailing_newline() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_missing_delimiter() {
    let content = "[x: 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]");
    assert_eq!(test.expectations.out, None);
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_delimiter_in_expected() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]  # comment with === in it";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(
        test.expectations.out.as_deref(),
        Some("[\"x\": 1]  # comment with === in it")
    );
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_error_section() {
    let content = "[call $error \"boom\"]\n=== error\n[E024]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[call $error \"boom\"]\n");
    assert_eq!(test.expectations.error.as_deref(), Some("[E024]"));
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_warn_section() {
    let content = "[x: 1]\n=== warn\ndeprecated feature used";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(
        test.expectations.warn.as_deref(),
        Some("deprecated feature used")
    );
}

#[test]
fn test_split_test_file_empty_content() {
    let content = "";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "");
    assert_eq!(test.expectations.out, None);
    assert!(!test.no_fs);
}

#[test]
fn test_split_test_file_multiple_sections() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]\n=== warn\ndeprecated\n=== error\nshould not reach";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert_eq!(test.expectations.warn.as_deref(), Some("deprecated"));
    assert_eq!(test.expectations.error.as_deref(), Some("should not reach"));
}

#[test]
fn test_split_test_file_bare_delimiter_returns_error() {
    let content = "[x: 1]\n===\n[\"x\": 1]";
    let result = split_test_file(content);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("bare '===' is no longer valid"));
}

#[test]
fn test_split_test_file_unknown_label_returns_error() {
    let content = "[x: 1]\n=== invalid\n[\"x\": 1]";
    let result = split_test_file(content);
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(err_msg.contains("unknown section label"));
    assert!(err_msg.contains("invalid"));
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

// ---------------------------------------------------------------------------
// Additional split_test_file() unit tests for Task 9
// ---------------------------------------------------------------------------

#[test]
fn test_split_test_file_out_section() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert!(test.expectations.warn.is_none());
    assert!(test.expectations.error.is_none());
}

#[test]
fn test_split_test_file_warn_section_only() {
    let content = "[x@UnknownType: 1]\n=== warn\n[W012] unknown type";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x@UnknownType: 1]\n");
    assert!(test.expectations.out.is_none());
    assert_eq!(
        test.expectations.warn.as_deref(),
        Some("[W012] unknown type")
    );
    assert!(test.expectations.error.is_none());
}

#[test]
fn test_split_test_file_error_section_only() {
    let content = "[call $error \"boom\"]\n=== error\n[E024] explicit error";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[call $error \"boom\"]\n");
    assert!(test.expectations.out.is_none());
    assert!(test.expectations.warn.is_none());
    assert_eq!(
        test.expectations.error.as_deref(),
        Some("[E024] explicit error")
    );
}

#[test]
fn test_split_test_file_all_three_sections() {
    let content = "[x: 1]\n=== out\n[\"x\": 1]\n=== warn\nwarning text\n=== error\nerror text";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert_eq!(test.expectations.warn.as_deref(), Some("warning text"));
    assert_eq!(test.expectations.error.as_deref(), Some("error text"));
}

#[test]
fn test_split_test_file_cap_net_directive() {
    let content = "# cap_net nc=*.local nc2=*.example.com\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert_eq!(test.expectations.out.as_deref(), Some("[\"x\": 1]"));
    assert!(!test.no_fs);
    assert_eq!(test.cap_net.len(), 2);
    assert_eq!(test.cap_net[0], ("nc".to_string(), "*.local".to_string()));
    assert_eq!(
        test.cap_net[1],
        ("nc2".to_string(), "*.example.com".to_string())
    );
}

#[test]
fn test_split_test_file_no_fs_and_cap_net_combined() {
    let content = "# no_fs cap_net nc=*.local\n[x: 1]\n=== out\n[\"x\": 1]";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    assert!(test.no_fs);
    assert_eq!(test.cap_net.len(), 1);
    assert_eq!(test.cap_net[0], ("nc".to_string(), "*.local".to_string()));
}

#[test]
fn test_split_test_file_empty_out_section() {
    let content = "[x: 1]\n=== out\n";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    // Empty section content becomes None (trimmed to empty string, then None)
    assert!(test.expectations.out.is_none());
}

#[test]
fn test_split_test_file_empty_warn_section() {
    let content = "[x: 1]\n=== warn\n";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    // Empty section content becomes None
    assert!(test.expectations.warn.is_none());
}

#[test]
fn test_split_test_file_empty_error_section() {
    let content = "[x: 1]\n=== error\n";
    let test = split_test_file(content).unwrap();
    assert_eq!(test.input, "[x: 1]\n");
    // Empty section content becomes None
    assert!(test.expectations.error.is_none());
}
