//! LSP diagnostics tests against the corpus test files.
//!
//! ## Task 10: LSP corpus runner
//!
//! Validates that the LSP produces zero diagnostics for "clean" corpus test files.
//! For each `.llt-eval` file in `tests/corpus/`:
//! - Skip files in `tests/corpus/invalid/`, `tests/corpus/eval/errors/`, and
//!   `tests/corpus/eval/type_errors/` directories (these have dedicated test runners)
//! - Skip files that have ANY `=== out`, `=== warn`, or `=== error` sections
//!   (these are owned by the eval corpus runner; matching LSP `DiagnosticSeverity::WARNING`
//!   against `=== warn` and `DiagnosticSeverity::ERROR` against `=== error` is planned
//!   future work, not yet implemented here)
//! - For remaining files (no sections, not in error directories):
//!   - Extract source content
//!   - Create a DocumentState (parse + expand + desugar + resolve + typecheck + eval)
//!   - Generate diagnostics via `diagnostics_for()`
//!   - Assert zero diagnostics (these files should be completely clean)
//!
//! This is STRICT enforcement — every corpus file is validated by at least one runner:
//! - Error directories → validated by their dedicated runners
//! - Files with `=== out/warn/error` → validated by eval corpus runner
//! - All other files → validated by this LSP test (must produce zero diagnostics)
//!
//! ## Task 11: LSP stdlib validation
//!
//! Validates that all `.llt` files under `stdlib/` parse without syntax errors.
//! Type checking is skipped because stdlib files reference prelude functions which
//! are not in the builtin type environment — they would produce false "undefined
//! variable" errors. The prelude itself is loaded and type-checked as part of
//! `create_stdlib_env()`, so stdlib correctness is validated at runtime.
//!
//! ## Implementation
//!
//! Uses `DocumentState::new()` + `diagnostics_for()` for corpus tests.
//! Uses `parse()` for stdlib syntax validation. Both approaches validate the
//! analysis pipeline without spawning the LSP server as a subprocess.

mod test_helpers;

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use test_helpers::split_test_file;

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

/// Simplified diagnostic for test comparisons.
#[derive(Debug, Clone)]
struct Diagnostic {
    severity: DiagnosticSeverity,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiagnosticSeverity {
    Error,
    Warning,
}

/// Get diagnostics for a source file by calling the LSP DocumentState pipeline directly.
#[cfg(feature = "lsp")]
fn get_diagnostics_for_source(source: &str) -> Vec<Diagnostic> {
    use tinct::lsp::analysis::diagnostics_for;
    use tinct::lsp::document::DocumentState;

    // Create minimal environment for LSP analysis
    let stdlib_env = tinct::create_stdlib_env().expect("Failed to create stdlib environment");
    let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .expect("Failed to open current directory");
    let eval_ctx = tinct::EvalContext::new(
        base_dir,
        Rc::clone(&stdlib_env),
        true, // no_fs (LSP context should have no_fs=true for security)
    );

    // Create DocumentState (this runs parse + expand + desugar + resolve + typecheck + eval)
    let doc = DocumentState::new(source.to_string(), &stdlib_env, &eval_ctx, None);

    // Generate LSP diagnostics
    let uri = "file:///test.llt"
        .parse::<lsp_types::Uri>()
        .expect("Failed to parse test URI");
    let lsp_diagnostics = diagnostics_for(&doc, &uri);

    // Convert to simplified diagnostic struct
    lsp_diagnostics
        .into_iter()
        .filter_map(|d| {
            let severity = match d.severity? {
                lsp_types::DiagnosticSeverity::ERROR => DiagnosticSeverity::Error,
                lsp_types::DiagnosticSeverity::WARNING => DiagnosticSeverity::Warning,
                _ => return None, // Ignore INFO/HINT for now
            };
            Some(Diagnostic {
                severity,
                message: d.message,
            })
        })
        .collect()
}

#[test]
#[cfg(feature = "lsp")]
fn test_lsp_corpus() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");

    // Exclude error-path directories (they have dedicated runners)
    let invalid_dir = corpus_dir.join("invalid");
    let eval_errors_dir = corpus_dir.join("eval/errors");
    let eval_type_errors_dir = corpus_dir.join("eval/type_errors");

    let test_files: Vec<_> = find_test_files(&corpus_dir)
        .into_iter()
        .filter(|p| {
            !p.starts_with(&invalid_dir)
                && !p.starts_with(&eval_errors_dir)
                && !p.starts_with(&eval_type_errors_dir)
        })
        .collect();

    assert!(
        !test_files.is_empty(),
        "No test files found in {}",
        corpus_dir.display()
    );

    let mut failed = Vec::new();
    let mut skipped = 0;
    let mut tested = 0;

    for test_file in &test_files {
        let content = fs::read_to_string(test_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", test_file.display(), e));

        let relative_path = test_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(test_file);

        let test = split_test_file(&content);

        // Skip files with ANY labeled sections — those are owned by the eval corpus runner
        if test.expectations.out.is_some()
            || test.expectations.warn.is_some()
            || test.expectations.error.is_some()
        {
            skipped += 1;
            continue;
        }

        tested += 1;

        // Get diagnostics from LSP
        let diagnostics = get_diagnostics_for_source(&test.input);

        // Files without sections must produce zero diagnostics
        if !diagnostics.is_empty() {
            let errors: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::Error)
                .collect();
            let warnings: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::Warning)
                .collect();

            let mut messages = Vec::new();
            if !errors.is_empty() {
                messages.extend(errors.iter().map(|d| format!("  [ERROR] {}", d.message)));
            }
            if !warnings.is_empty() {
                messages.extend(
                    warnings
                        .iter()
                        .map(|d| format!("  [WARNING] {}", d.message)),
                );
            }

            failed.push((
                relative_path.to_path_buf(),
                format!(
                    "Expected zero diagnostics (file has no === sections):\n{}",
                    messages.join("\n")
                ),
            ));
        }
    }

    if !failed.is_empty() {
        eprintln!(
            "\n{} LSP corpus test(s) failed ({} tested, {} skipped):",
            failed.len(),
            tested,
            skipped
        );
        for (path, error) in &failed {
            eprintln!("  - {}: {}", path.display(), error);
        }
        panic!("LSP corpus tests failed");
    }

    eprintln!(
        "LSP corpus: {} files tested, {} files skipped (owned by eval corpus runner)",
        tested, skipped
    );
}

#[test]
#[cfg(feature = "lsp")]
fn test_lsp_stdlib_clean() {
    let stdlib_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");

    // Find all .llt files under stdlib/
    let mut stdlib_files = Vec::new();
    for entry in walkdir::WalkDir::new(&stdlib_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("llt") {
            stdlib_files.push(path.to_path_buf());
        }
    }

    assert!(
        !stdlib_files.is_empty(),
        "No stdlib .llt files found in {}",
        stdlib_dir.display()
    );

    let mut failed = Vec::new();

    for stdlib_file in &stdlib_files {
        let content = fs::read_to_string(stdlib_file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", stdlib_file.display(), e));

        let relative_path = stdlib_file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(stdlib_file);

        // Parse the file to check for syntax errors.
        // Type checking is skipped because stdlib files reference prelude functions
        // which are not in the builtin type environment — they would produce false
        // "undefined variable" errors. The prelude itself is loaded and type-checked
        // as part of create_stdlib_env(), so stdlib correctness is validated at runtime.
        match tinct::parse(&content) {
            Ok(_) => {
                // Parse succeeded, no syntax errors
            }
            Err(error) => {
                failed.push((relative_path.to_path_buf(), format!("{}", error)));
            }
        }
    }

    if !failed.is_empty() {
        eprintln!("\n{} stdlib file(s) have parse errors:", failed.len());
        for (path, error) in &failed {
            eprintln!("  - {}:\n{}", path.display(), error);
        }
        panic!("Stdlib files must parse without errors");
    }
}
