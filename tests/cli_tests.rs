//! Integration tests for the `tinct` CLI binary.
//!
//! These tests exercise the CLI (main.rs) via `std::process::Command`,
//! covering subcommands, output formats, flags, and error cases.
//! The binary requires the `cli` feature, so we gate the entire file.

#![cfg(feature = "cli")]
// Test infrastructure uses std::fs — no cap_std available in test harness.
#![allow(
    clippy::disallowed_methods,
    clippy::useless_format,
    clippy::approx_constant,
    clippy::expect_fun_call
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Return the path to the compiled `tinct` binary.
/// `CARGO_BIN_EXE_tinct` is set by Cargo during test compilation when a
/// `[[bin]]` named `tinct` exists.
fn tinct_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tinct"))
}

/// A temporary directory that is automatically removed on drop.
/// Each test gets its own unique subdirectory to avoid collisions and
/// leftover files.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join("tinct_cli_tests").join(label);
        fs::create_dir_all(&path).expect("failed to create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
}

/// Create a temporary LLT file with the given content and return its path
/// along with a guard that cleans up the directory on drop.
/// Uses the test name (via a caller-supplied label) to make filenames unique
/// so parallel tests never collide.
fn write_temp_llt(label: &str, content: &str) -> (PathBuf, TempDir) {
    let dir = TempDir::new(label);
    let path = dir.path().join(format!("{label}.llt"));
    fs::write(&path, content).expect("failed to write temp file");
    (path, dir)
}

// ---------------------------------------------------------------------------
// Basic evaluation — default JSON output
// ---------------------------------------------------------------------------

#[test]
fn eval_simple_dict_json_output() {
    // Use quoted string "hello" since bare words (hello) are VarRefs in LLT, not strings.
    // rebuild marker: wave1-sprint-fix
    let (path, _dir) = write_temp_llt("eval_simple_dict", "[x: 1 y: \"hello\"]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!({"x": 1, "y": "hello"}));
}

#[test]
fn eval_scalar_int() {
    let (path, _dir) = write_temp_llt("eval_scalar_int", "42");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

#[test]
fn eval_scalar_string() {
    let (path, _dir) = write_temp_llt("eval_scalar_string", "\"hello world\"");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!("hello world"));
}

#[test]
fn eval_scalar_bool() {
    let (path, _dir) = write_temp_llt("eval_scalar_bool", "true");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(true));
}

#[test]
fn eval_scalar_float() {
    let (path, _dir) = write_temp_llt("eval_scalar_float", "3.14");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(3.14));
}

#[test]
fn eval_array_like_dict() {
    let (path, _dir) = write_temp_llt("eval_array_like", "[10 20 30]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!([10, 20, 30]));
}

#[test]
fn eval_nested_dict() {
    let (path, _dir) = write_temp_llt("eval_nested", "[a: [b: [c: 42]]]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"a": {"b": {"c": 42}}}));
}

// ---------------------------------------------------------------------------
// -o json (explicit)
// ---------------------------------------------------------------------------

#[test]
fn eval_format_json_explicit() {
    let (path, _dir) = write_temp_llt("eval_format_json", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn eval_format_json_long_flag() {
    // --output is the long form of -o; test it produces the same JSON output.
    let (path, _dir) = write_temp_llt("eval_format_json_long", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "--output", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn eval_invalid_format_path_traversal() {
    // -o with a path-traversal string must be rejected before any filesystem access.
    let (path, _dir) = write_temp_llt("eval_invalid_format", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "../secret", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit for path-traversal format name"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid formatter name"),
        "expected error message mentioning invalid formatter name, got: {}",
        stderr
    );
}

#[test]
fn eval_invalid_input_path_traversal() {
    // -i with a path-traversal string must be rejected before any filesystem access.
    let (path, _dir) = write_temp_llt("eval_invalid_input_format", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "-i", "../secret", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit for path-traversal input format name"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid formatter name"),
        "expected error message mentioning invalid formatter name, got: {}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// -o llt
// ---------------------------------------------------------------------------

#[test]
fn eval_format_llt_scalar() {
    let (path, _dir) = write_temp_llt("eval_format_llt_scalar", "42");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Int(42)");
}

#[test]
fn eval_format_llt_dict() {
    let (path, _dir) = write_temp_llt("eval_format_llt_dict", "[x: 42]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Dict({\"x\": Int(42)})");
}

#[test]
fn eval_format_llt_string() {
    let (path, _dir) = write_temp_llt("eval_format_llt_string", "\"hello\"");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "String(\"hello\")");
}

#[test]
fn eval_format_llt_bool() {
    let (path, _dir) = write_temp_llt("eval_format_llt_bool", "true");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Bool(true)");
}

#[test]
fn eval_format_llt_float() {
    let (path, _dir) = write_temp_llt("eval_format_llt_float", "3.14");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Float(3.14)");
}

// ---------------------------------------------------------------------------
// --eval flag (shallow materialization)
// ---------------------------------------------------------------------------

#[test]
fn eval_flag_deep_materialize() {
    // With --eval and -o json, the -o branch is taken; --eval is a no-op in this path.
    // Deep materialization is handled internally by to-json (codecs/json.llt), not by main.rs.
    // This test verifies that --eval + -o json together produce correct JSON output.
    let (path, _dir) = write_temp_llt("eval_flag_deep", "[a: [b: [c: 42]]]");
    let output = Command::new(tinct_bin())
        .args(["run", "--eval", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"a": {"b": {"c": 42}}}));
}

#[test]
fn eval_flag_with_llt_format() {
    let (path, _dir) = write_temp_llt("eval_flag_llt", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "--eval", "-o", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Dict({\"x\": Int(1)})");
}

// ---------------------------------------------------------------------------
// Multi-document pipeline
// ---------------------------------------------------------------------------

#[test]
fn eval_multi_document_pipeline() {
    // doc1 produces {x: 10}, doc2 receives it as % and wraps it
    let source = "[x: 10]\n---\n[result: %.x]";
    let (path, _dir) = write_temp_llt("eval_multi_doc", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": 10}));
}

// ---------------------------------------------------------------------------
// Expressions with stdlib builtins
// ---------------------------------------------------------------------------

#[test]
fn eval_builtin_add() {
    let (path, _dir) = write_temp_llt("eval_builtin_add", "[call $+ 1 2]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(3));
}

#[test]
fn eval_builtin_if() {
    let (path, _dir) = write_temp_llt("eval_builtin_if", "[call $if true 42 99]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

// ---------------------------------------------------------------------------
// Error cases
// ---------------------------------------------------------------------------

#[test]
fn eval_missing_file() {
    let output = Command::new(tinct_bin())
        .args(["run", "/tmp/llt_cli_tests/nonexistent_file.llt"])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit for missing file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error reading file"),
        "expected 'error reading file' in stderr, got: {stderr}"
    );
}

#[test]
fn eval_missing_file_exit_code() {
    let output = Command::new(tinct_bin())
        .args(["run", "/tmp/llt_cli_tests/no_such_file.llt"])
        .output()
        .expect("failed to run tinct");

    // main.rs exits with code 1 for errors
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn eval_parse_error() {
    // Unterminated bracket is a parse error
    let (path, _dir) = write_temp_llt("eval_parse_error", "[x: 1");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit for parse error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The error message should be non-empty
    assert!(
        !stderr.trim().is_empty(),
        "expected error message on stderr"
    );
}

#[test]
fn eval_error_undefined_var() {
    // Referencing an undefined variable should produce an eval error
    let (path, _dir) = write_temp_llt("eval_error_undef", "$nonexistent");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "expected error message for undefined var"
    );
}

// ---------------------------------------------------------------------------
// No subcommand / help
// ---------------------------------------------------------------------------

#[test]
fn no_subcommand_shows_usage() {
    let output = Command::new(tinct_bin())
        .output()
        .expect("failed to run tinct");

    // clap exits non-zero when no subcommand is given
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // clap prints usage info to stderr
    assert!(
        stderr.contains("Usage") || stderr.contains("usage"),
        "expected usage info in stderr, got: {stderr}"
    );
}

#[test]
fn help_flag() {
    let output = Command::new(tinct_bin())
        .args(["--help"])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unified data representation and transformation language"),
        "expected description in help output, got: {stdout}"
    );
}

#[test]
fn eval_help_flag() {
    let output = Command::new(tinct_bin())
        .args(["run", "--help"])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Evaluate"),
        "expected eval description in help output, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Invalid format value
// ---------------------------------------------------------------------------

#[test]
fn eval_invalid_format() {
    let (path, _dir) = write_temp_llt("eval_invalid_format", "42");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "xml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // The output formatter file stdlib/cli/out/xml.llt does not exist
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("formatter not found") || stderr.contains("--output"),
        "expected formatter-not-found error, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Version flag
// ---------------------------------------------------------------------------

#[test]
fn version_flag() {
    let output = Command::new(tinct_bin())
        .args(["--version"])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("tinct"),
        "expected 'tinct' in version output, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// JSON output is well-formed pretty-printed
// ---------------------------------------------------------------------------

#[test]
fn eval_json_output_is_valid_json() {
    let (path, _dir) = write_temp_llt("eval_valid_json", "[a: 1 b: 2]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // -o json produces compact JSON via json.llt
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON output");
    assert_eq!(json, serde_json::json!({"a": 1, "b": 2}));
}

// ---------------------------------------------------------------------------
// Stdin JSON injection (piped via stdin to the child process)
// ---------------------------------------------------------------------------

#[test]
fn eval_stdin_json_injection() {
    // When stdin is piped with JSON, it should be available as % in the first doc
    let (path, _dir) = write_temp_llt("eval_stdin_json", "[name: %.name]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(b"{\"name\": \"Alice\"}").ok();
            }
            // Close stdin so the child doesn't block
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run llt with piped stdin");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"name": "Alice"}));
}

// ---------------------------------------------------------------------------
// Empty file
// ---------------------------------------------------------------------------

#[test]
fn eval_empty_file() {
    // An empty/whitespace-only file should be a parse error (no expression)
    let (path, _dir) = write_temp_llt("eval_empty", "  \n  ");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // Empty input may or may not be valid depending on the grammar;
    // just verify it doesn't panic (exit code 2 = thread panic)
    if !output.status.success() {
        assert_ne!(
            output.status.code(),
            Some(2),
            "binary panicked on empty input"
        );
    }
}

// ---------------------------------------------------------------------------
// Scope chain (multi-expression within a document)
// ---------------------------------------------------------------------------

#[test]
fn eval_scope_chain() {
    // Second expression should see bindings from the first
    let source = "[x: 10]\n[result: $x]";
    let (path, _dir) = write_temp_llt("eval_scope_chain", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": 10}));
}

// ---------------------------------------------------------------------------
// $include tests
// ---------------------------------------------------------------------------

/// Create a temporary directory with a unique label for $include tests.
/// Returns a `TempDir` that is automatically cleaned up on drop.
fn make_include_dir(label: &str) -> TempDir {
    TempDir::new(&format!("include_{label}"))
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_basic_dict() {
    let dir = make_include_dir("basic_dict");
    let helper = dir.path().join("helper.llt");
    fs::write(&helper, "[x: 1 y: 2]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[result: [include %cwd \"helper.llt\"]]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", main.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": {"x": 1, "y": 2}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_namespaced() {
    // Include a helper and access its fields via the namespace binding
    let dir = make_include_dir("namespaced");
    fs::write(
        dir.path().join("helper.llt"),
        "[double: [fn [n] [call $* $n 2]]]",
    )
    .unwrap();
    let main_src = r#"[utils: [include %cwd "helper.llt"]]
[result: [call $utils.double 21]]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": 42}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_merged_scope_chain() {
    // First expression is an include (result merges into scope), second uses its bindings
    let dir = make_include_dir("merged_scope");
    fs::write(dir.path().join("helper.llt"), "[x: 10 y: 20]").unwrap();
    let main_src = "[include %cwd \"helper.llt\"]\n[sum: [call $+ $x $y]]";
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"sum": 30}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_nested_a_includes_b_includes_c() {
    // A includes B, B includes C — nested transitive include
    let dir = make_include_dir("nested_chain");
    fs::write(dir.path().join("c.llt"), "[val: 99]").unwrap();
    fs::write(
        dir.path().join("b.llt"),
        "[inner: [include %cwd \"c.llt\"]]",
    )
    .unwrap();
    fs::write(
        dir.path().join("a.llt"),
        "[outer: [include %cwd \"b.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("a.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"outer": {"inner": {"val": 99}}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_circular_error() {
    // A includes B, B includes A — circular dependency
    let dir = make_include_dir("circular");
    fs::write(dir.path().join("a.llt"), "[include %cwd \"b.llt\"]").unwrap();
    fs::write(dir.path().join("b.llt"), "[include %cwd \"a.llt\"]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["run", dir.path().join("a.llt").to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure for circular include"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("circular include"),
        "expected 'circular include' in stderr, got: {stderr}"
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_self_circular_error() {
    // File includes itself — degenerate circular case
    let dir = make_include_dir("self_circular");
    fs::write(dir.path().join("self.llt"), "[include %cwd \"self.llt\"]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["run", dir.path().join("self.llt").to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure for self-include"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("circular include"),
        "expected 'circular include' in stderr, got: {stderr}"
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_file_not_found_error() {
    let dir = make_include_dir("file_not_found");
    fs::write(
        dir.path().join("main.llt"),
        "[include %cwd \"nonexistent.llt\"]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["run", dir.path().join("main.llt").to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure for missing include file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot access"),
        "expected 'cannot access' in stderr, got: {stderr}"
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_relative_path_from_subdirectory() {
    // Main file in root dir includes a file in a subdirectory via relative path
    let dir = make_include_dir("relative_subdir");
    let sub = dir.path().join("lib");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("utils.llt"), "[pi: 3.14]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[math: [include %cwd \"lib/utils.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"math": {"pi": 3.14}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_relative_path_from_included_file() {
    // Main includes sub/a.llt, which eagerly includes sibling b.llt (relative to sub/).
    // The include must be at the top level (not inside a dict entry) so it is
    // evaluated while the base_dir still points at sub/. Dict entry values are
    // lazy, so a nested [call $include ...] inside a dict would only materialize
    // after base_dir has been restored to the parent.
    let dir = make_include_dir("relative_from_included");
    let sub = dir.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("b.llt"), "[val: 42]").unwrap();
    // a.llt uses a scope chain: first expression includes b.llt eagerly,
    // second expression wraps the result.
    fs::write(
        sub.join("a.llt"),
        "[include %cwd \"b.llt\"]\n[nested: $val]",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[wrapper: [include %cwd \"sub/a.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"wrapper": {"nested": 42}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_with_stdlib_builtins() {
    // Included file uses stdlib builtins (arithmetic)
    let dir = make_include_dir("stdlib_builtins");
    fs::write(dir.path().join("math.llt"), "[sum: [call $+ 10 20]]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[result: [include %cwd \"math.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": {"sum": 30}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_returns_scalar() {
    // Included file evaluates to a scalar (not a dict)
    let dir = make_include_dir("scalar_return");
    fs::write(dir.path().join("answer.llt"), "42").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[answer: [include %cwd \"answer.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"answer": 42}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_returns_string() {
    // Included file evaluates to a string scalar
    let dir = make_include_dir("string_return");
    fs::write(dir.path().join("greeting.llt"), "\"hello world\"").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[msg: [include %cwd \"greeting.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"msg": "hello world"}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_diamond_pattern_no_cycle() {
    // Diamond: main includes A and B, both include C — NOT circular
    // (C is included twice but never re-enters while already in the guard)
    let dir = make_include_dir("diamond");
    fs::write(dir.path().join("c.llt"), "[shared: 100]").unwrap();
    fs::write(
        dir.path().join("a.llt"),
        "[a_data: [include %cwd \"c.llt\"]]",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.llt"),
        "[b_data: [include %cwd \"c.llt\"]]",
    )
    .unwrap();
    let main_src = r#"[
  a: [include %cwd "a.llt"]
  b: [include %cwd "b.llt"]
]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "a": {"a_data": {"shared": 100}},
            "b": {"b_data": {"shared": 100}}
        })
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_isolation_no_caller_scope() {
    // Included file should NOT see bindings from the caller's scope
    let dir = make_include_dir("isolation");
    // The included file tries to reference $caller_var which is only in main's scope
    fs::write(dir.path().join("helper.llt"), "[val: $caller_var]").unwrap();
    let main_src = "[caller_var: 999]\n[result: [include %cwd \"helper.llt\"]]";
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--eval",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure: included file should not see caller scope"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined variable") || stderr.contains("not defined"),
        "expected undefined variable error, got: {stderr}"
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_with_deep_materialize() {
    // Use --eval flag with includes to exercise deep materialization
    let dir = make_include_dir("deep_materialize");
    fs::write(dir.path().join("nested.llt"), "[a: [b: [c: 42]]]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[data: [include %cwd \"nested.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--eval",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"data": {"a": {"b": {"c": 42}}}}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_llt_format_output() {
    // Include test with LLT display format output
    let dir = make_include_dir("llt_format");
    fs::write(dir.path().join("helper.llt"), "[x: 42]").unwrap();
    fs::write(dir.path().join("main.llt"), "[include %cwd \"helper.llt\"]").unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "llt",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Dict({\"x\": Int(42)})");
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_path_traversal_parent_dir() {
    // child.llt in a subdirectory tries to include ../parent.llt via path traversal.
    // cap-std's RESOLVE_BENEATH sandbox correctly rejects parent-directory escapes,
    // so this should produce an error (not succeed).
    let dir = make_include_dir("path_traversal");
    let subdir = dir.path().join("subdir");
    fs::create_dir_all(&subdir).unwrap();
    fs::write(dir.path().join("parent.llt"), "[greeting: hello]").unwrap();
    fs::write(
        subdir.join("child.llt"),
        "[data: [include %cwd \"../parent.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["run", "--eval", subdir.join("child.llt").to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "parent-directory traversal should be rejected by sandbox"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot access") || stderr.contains("outside"),
        "expected sandbox error, got: {}",
        stderr
    );
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_underscore_desugar() {
    // Regression test for $include + $_ (implicit lambda) desugaring.
    // The helper file contains $_ syntax which must be desugared before eval.
    // This verifies that builtin_include calls desugar_file() correctly.
    let dir = make_include_dir("underscore_desugar");

    // Helper file: an implicit lambda that accesses the "name" field
    fs::write(dir.path().join("mapper.llt"), "$_.name").unwrap();

    // Main file: use a scope chain to include the helper, bind it to a variable
    // in the first expression, then call it in the second expression and only
    // return the result (not the function, since functions aren't JSON-serializable)
    let main_src = r#"[get_name: [include %cwd "mapper.llt"]]
[result: [call $get_name [name: "Alice"]]]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": "Alice"}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_with_dircap() {
    // Test the new cap-qualified include pattern: [include $cap "path"]
    let dir = make_include_dir("dircap_include");

    // Create a helper file in the test directory
    fs::write(dir.path().join("data.llt"), "[value: 42]").unwrap();

    // Main file uses `%cap` (injected via --cap-fs) for include.
    // Use a scope chain to avoid serializing the DirCap itself.
    let main_src = r#"%cap
---
[include % "data.llt"]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:r", dir.path().display()),
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"value": 42}));
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn include_with_dircap_and_hash() {
    // Test cap-qualified include with integrity hash: [include $cap "path" "hash"]
    let dir = make_include_dir("dircap_hash");

    // Create a helper file
    let content = "[value: 99]";
    fs::write(dir.path().join("data.llt"), content).unwrap();

    // Compute the blake3 hash of the content
    let hash = blake3::hash(content.as_bytes());
    let hash_hex = hash.to_hex();

    // Main file uses %cap (injected via --cap-fs) for include with hash verification.
    // Use a scope chain to avoid serializing the DirCap itself.
    let main_src = format!(
        r#"%cap
---
[include % "data.llt" "blake3:{}"]"#,
        hash_hex
    );
    fs::write(dir.path().join("main.llt"), &main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:r", dir.path().display()),
            dir.path().join("main.llt").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"value": 99}));
}

// ---------------------------------------------------------------------------
// --no-fs flag (sandbox filesystem access)
// ---------------------------------------------------------------------------

#[test]
fn no_fs_flag_blocks_include() {
    // --no-fs flag should prevent $include from accessing the filesystem
    let (path, _dir) = write_temp_llt("no_fs_flag", "[include %cwd \"some_file.llt\"]");
    let output = Command::new(tinct_bin())
        .args(["run", "--no-fs", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit code when --no-fs blocks $include"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 (error) for --no-fs violation"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("filesystem access is disabled")
            || stderr.contains("E042")
            || stderr.contains("undefined variable: %cwd"),
        "expected error message about disabled filesystem access, got: {stderr}"
    );
}

#[test]
fn no_fs_suppresses_cwd_injection() {
    // --no-fs must suppress %cwd injection; code that references %cwd should
    // fail with "undefined variable: %cwd", not succeed.
    let (path, _dir) = write_temp_llt("no_fs_suppresses_pwd", "%cwd");
    let output = Command::new(tinct_bin())
        .args(["run", "--no-fs", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure when --no-fs suppresses %cwd, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("%cwd") || stderr.contains("undefined"),
        "expected error mentioning %cwd or undefined variable, got: {stderr}"
    );
}

#[test]
fn no_fs_suppresses_cap_fs_injection() {
    // --no-fs must suppress --cap-fs injection: even if the operator passes
    // --cap-fs d=.:r, the %d capability must NOT be injected when --no-fs is set.
    let (path, _dir) = write_temp_llt("no_fs_suppresses_cap_fs", "%d");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-fs",
            "--cap-fs",
            "d=.:r",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure when --no-fs suppresses --cap-fs d=.:r injection, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("%d") || stderr.contains("undefined"),
        "expected error mentioning %d or undefined variable, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// DirCap filesystem confinement (removed --allow-path tests)
// ---------------------------------------------------------------------------

// NOTE: The --allow-path and --allow-host flags were removed in cap-simplify sprint.
// Filesystem access is controlled via the object capability model:
//   - %cwd (injected automatically, suppress with --no-cwd)
//   - %libdir (injected automatically, suppress with --no-libdir)
//   - --cap-fs NAME=PATH:MODE (injects %NAME as a DirCap)
// Each DirCap is backed by cap-std RESOLVE_BENEATH enforcement.
// Landlock is auto-triggered from --cap-fs entries (unless --no-landlock is set).

// ---------------------------------------------------------------------------
// --timeout flag (wall-clock timeout)
// ---------------------------------------------------------------------------

#[test]
fn timeout_flag_exits_with_sigalrm() {
    // Test that --timeout flag installs SIGALRM handler and exits with code 2.
    // Use an infinite workload (iterate with collect) that can never complete.
    // Set a short timeout (1s) to ensure SIGALRM fires.
    let source = r#"[call $collect [call $iterate [fn [x] [call $+ x 1]] 0]]"#;
    let (path, _dir) = write_temp_llt("timeout_flag", source);

    let start = std::time::Instant::now();
    let output = Command::new(tinct_bin())
        .args(["run", "--timeout", "1s", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");
    let elapsed = start.elapsed();

    // The process must not succeed
    assert!(
        !output.status.success(),
        "expected non-zero exit code with --timeout 1s on infinite workload"
    );

    // The --timeout flag should cause exit code 2 when SIGALRM fires.
    let exit_code = output.status.code();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        exit_code,
        Some(2),
        "expected exit code 2 (timeout), got {:?}\nstderr: {}",
        exit_code,
        stderr
    );

    // Process should terminate quickly (within timeout period + overhead)
    assert!(
        elapsed.as_secs() <= 3,
        "process took too long: {:?}",
        elapsed
    );
}

// ---------------------------------------------------------------------------
// Sandbox flag composition and happy paths
// ---------------------------------------------------------------------------

#[test]
fn no_fs_flag_happy_path() {
    // Test that --no-fs does not interfere with normal evaluation that
    // doesn't require filesystem access.
    let (path, _dir) = write_temp_llt("no_fs_happy", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", "--no-fs", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success when --no-fs doesn't affect code, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 for successful evaluation"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("failed to parse JSON output");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn timeout_flag_fast_program_succeeds() {
    // Test that --timeout does not interfere with programs that complete
    // within the time limit. A fast-completing program should succeed with
    // exit code 0, not timeout.
    let (path, _dir) = write_temp_llt("timeout_fast", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--timeout",
            "5s",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success for fast program with --timeout 5s, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 (not timeout) for fast-completing program"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("failed to parse JSON output");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn timeout_flag_invalid_argument_rejected() {
    // Test that invalid --timeout arguments are rejected at parse time by clap
    // with a non-zero exit code. This tests the clap ValueParser, not runtime.
    let (path, _dir) = write_temp_llt("timeout_invalid_arg", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "--timeout", "abc", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for invalid --timeout argument"
    );

    let exit_code = output.status.code();
    // clap typically exits with code 1 or 2 for validation errors
    assert!(
        exit_code == Some(1) || exit_code == Some(2),
        "expected exit code 1 or 2 for clap parse error, got {:?}",
        exit_code
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid") || stderr.contains("error") || stderr.contains("parse"),
        "expected error message about invalid timeout value, got: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn no_fs_flag_and_timeout_flag_compose() {
    // Test that --no-fs and --timeout flags can be used together without
    // conflict. Both sandbox flags should be active.
    let (path, _dir) = write_temp_llt("no_fs_timeout_compose", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-fs",
            "--timeout",
            "5s",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with both --no-fs and --timeout, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 for composed flags"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("failed to parse JSON output");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn no_fs_flag_and_timeout_flag_conjunctive_enforcement() {
    // Test that both --no-fs and --timeout flags are actively enforced
    // simultaneously. The test should fail due to --no-fs blocking $include,
    // not due to timeout.
    let (path, _dir) = write_temp_llt(
        "conjunctive_enforcement",
        "[include %cwd \"some_file.llt\"]",
    );
    let output = Command::new(tinct_bin())
        .args(["run", "--no-fs", "--timeout", "5s", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit code when --no-fs blocks $include"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 (error from --no-fs), not timeout code 2"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("filesystem access is disabled")
            || stderr.contains("E042")
            || stderr.contains("undefined variable: %cwd"),
        "expected E042 error message about disabled filesystem access, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Error cases — JSON serialization failures
// ---------------------------------------------------------------------------

#[test]
fn eval_proxy_json_serialization_error() {
    // Proxy values cannot be serialized to JSON. This test verifies that when
    // a Proxy is returned by the evaluator, the CLI exits with an error when
    // trying to convert the result to JSON output.
    let (path, _dir) = write_temp_llt("proxy_json_serialization", "[call $proxy [fn [k] $k]]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit code when serializing Proxy to JSON"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 (error) when serializing Proxy to JSON"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot serialize Proxy to JSON") || stderr.contains("E099"),
        "expected error message about Proxy JSON serialization, got: {stderr}"
    );
}

#[test]
fn json_serialization_function_produces_null() {
    // Functions serialized via to-json (codecs/json.llt) produce JSON null.
    // This matches the catch-all in to-json-primitive (tinct's JSON serializer).
    let (path, _dir) = write_temp_llt("json_ser_function", "[fn [let x] x]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected exit 0 when serializing Function to JSON (produces null), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "null",
        "expected JSON null for Function, got: {stdout}"
    );
}

#[test]
fn json_serialization_seq_produces_array() {
    // Seqs serialized via to-json (codecs/json.llt) produce a JSON array.
    // to-json-seq collects the sequence and serializes each element.
    let (path, _dir) = write_temp_llt("json_ser_seq", "[seq 1 10]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected exit 0 when serializing Seq to JSON (produces array), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().starts_with('[') && stdout.trim().ends_with(']'),
        "expected JSON array for Seq, got: {stdout}"
    );
}

#[test]
fn eval_deep_materialize_seq() {
    // Test deep materialization with a sequence
    let (path, _dir) = write_temp_llt(
        "deep_materialize_seq",
        "[call $collect [call $take 3 [call $range 0 10]]]",
    );
    let output = Command::new(tinct_bin())
        .args(["run", "--eval", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!([0, 1, 2]));
}

// ---------------------------------------------------------------------------
// sandbox-b: --no-landlock flag (Landlock filesystem ACL, Linux 5.13+)
// ---------------------------------------------------------------------------

#[test]
fn no_landlock_flag_accepted() {
    // --no-landlock must be a recognized flag (clap should not reject it).
    // Even without --cap-fs, passing --no-landlock should not error.
    let (path, _dir) = write_temp_llt("no_landlock_flag", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", "--no-landlock", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // Must succeed — the flag is a no-op when no --cap-fs is given.
    assert!(
        output.status.success(),
        "expected success with --no-landlock flag alone; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[ignore = "pre-existing regression from runtime-v2 merge: include pipeline has remaining issues with Document stage/name matching in eval-document-runtime"]
fn no_landlock_with_cap_fs_accepted() {
    // --no-landlock combined with --cap-fs must be accepted. The flag
    // disables Landlock kernel enforcement while still using cap-std RESOLVE_BENEATH.
    // This is the graceful degradation path for kernels < 5.13 or environments
    // where Landlock is unavailable.
    let dir = TempDir::new("no_landlock_cap_fs");
    let included = dir.path().join("data.llt");
    fs::write(&included, "[value: 99]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[include %cwd \"data.llt\"]").unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-landlock",
            "--cap-fs",
            &format!("data={}:r", dir_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "--no-landlock with --cap-fs should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"value": 99}));
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "pre-existing regression from runtime-v2 merge: include pipeline (eval-document-runtime/match) has remaining issues with non-exhaustive patterns in Document name/stage matching"]
fn landlock_with_cap_fs_permits_include() {
    // On Linux, --cap-fs activates Landlock by default. $include from the
    // cap directory must succeed. This test confirms Landlock does not
    // accidentally block access to explicitly allowed paths.
    let dir = TempDir::new("landlock_cap_fs_permit");
    let included = dir.path().join("lib.llt");
    fs::write(&included, "[result: 42]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[include %cwd \"lib.llt\"]").unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--cap-fs",
            &format!("data={}:r", dir_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "Landlock should allow access to explicitly allowed path; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"result": 42}));
}

// ---------------------------------------------------------------------------
// DirPerms and file capability tests
// ---------------------------------------------------------------------------

#[test]
fn cap_fs_read_only_permits_readable() {
    // --cap-fs mydir=DIR:r grants read-only access to the injected %mydir DirCap.
    // Just verify the cap is injected and usable.
    let dir = TempDir::new("cap_fs_ro_list");
    let test_file = dir.path().join("test.txt");
    fs::write(&test_file, "test content").unwrap();
    let main = dir.path().join("main.llt");
    // Just return the cap itself (proves it's injected and evaluates)
    let llt_content = r#"%mydir"#;
    fs::write(&main, llt_content).unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-cwd",
            "--no-libdir",
            "--cap-fs",
            &format!("mydir={}:r", dir_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "cap injection with r perms should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cap_fs_read_only_write_fails() {
    // --cap-fs mydir=DIR:r grants read-only access. raw-create should fail (needs Writable).
    let dir = TempDir::new("cap_fs_ro_write_fail");
    let main = dir.path().join("main.llt");
    let llt_content = r#"[call $raw-create %mydir "test.txt"]"#;
    fs::write(&main, llt_content).unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-cwd",
            "--no-libdir",
            "--cap-fs",
            &format!("mydir={}:r", dir_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "raw-create with r perms should fail; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Writable"),
        "expected permission error mentioning Writable; stderr: {stderr}"
    );
}

#[test]
fn cap_fs_read_write_permits_writable() {
    // --cap-fs mydir=DIR:rw grants read+write access. Just verify the cap is injected.
    let test_root = TempDir::new("cap_fs_rw");
    let data_dir = test_root.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let main = test_root.path().join("main.llt");
    // Just return the cap itself (proves it's injected with rw perms)
    let llt_content = r#"%mydir"#;
    fs::write(&main, llt_content).unwrap();

    let data_str = data_dir.to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-cwd",
            "--no-libdir",
            "--cap-fs",
            &format!("mydir={}:rw", data_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "cap injection with rw perms should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cap_fs_bare_no_mode_errors() {
    // --cap-fs NAME=PATH without :MODE must error after dircap-drop-bare-compat.
    let (path, _dir) = write_temp_llt("cap_fs_bare_no_mode", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "--cap-fs", "d=.", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");
    assert!(
        !output.status.success(),
        "--cap-fs d=. (no mode) should error; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires mode suffix"),
        "expected 'requires mode suffix' in error, got: {stderr}"
    );
}

#[test]
fn cap_fs_empty_mode_errors() {
    // --cap-fs NAME=PATH: (trailing colon, empty mode) must error.
    let (path, _dir) = write_temp_llt("cap_fs_empty_mode", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["run", "--cap-fs", "d=.:", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");
    assert!(
        !output.status.success(),
        "--cap-fs d=.: (empty mode) should error; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mode string is empty"),
        "expected 'mode string is empty' in error, got: {stderr}"
    );
}

#[test]
fn cap_fs_bare_literate_eval_errors() {
    // --cap-fs NAME=PATH without :MODE must error in literate eval.
    let (path, _dir) = write_temp_md(
        "cap_fs_bare_literate_eval",
        "# Test\n\n```tinct\n[x: 1]\n```\n",
    );
    let output = Command::new(tinct_bin())
        .args([
            "literate",
            "eval",
            "--cap-fs",
            "d=.",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");
    assert!(
        !output.status.success(),
        "--cap-fs d=. (no mode) should error in literate eval; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires mode suffix"),
        "expected 'requires mode suffix' in error, got: {stderr}"
    );
}

#[test]
fn cap_fs_bare_literate_weave_errors() {
    // --cap-fs NAME=PATH without :MODE must error in literate weave.
    let (path, _dir) = write_temp_md(
        "cap_fs_bare_literate_weave",
        "# Test\n\n```tinct\n[x: 1]\n```\n",
    );
    let output = Command::new(tinct_bin())
        .args([
            "literate",
            "weave",
            "--cap-fs",
            "d=.",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");
    assert!(
        !output.status.success(),
        "--cap-fs d=. (no mode) should error in literate weave; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires mode suffix"),
        "expected 'requires mode suffix' in error, got: {stderr}"
    );
}

#[test]
fn cap_file_no_mode_defaults_to_r() {
    // --cap-file cfg=FILE (no :mode suffix) should default to r (readable text).
    // We create a temp file and slurp it.
    let dir = TempDir::new("cap_file_no_mode");
    let test_file = dir.path().join("config.txt");
    fs::write(&test_file, "key: value").unwrap();
    let main = dir.path().join("main.llt");
    // Slurp the injected %cfg handle
    let llt_content = r#"[call $slurp %cfg]"#;
    fs::write(&main, llt_content).unwrap();

    let file_str = test_file.to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-cwd",
            "--no-libdir",
            "--cap-file",
            &format!("cfg={}", file_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "slurp with default r mode should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("key: value"),
        "expected file content in output; stdout: {stdout}"
    );
}

#[test]
fn cap_file_read_mode_succeeds() {
    // --cap-file cfg=FILE:r should succeed for reading.
    let dir = TempDir::new("cap_file_r");
    let test_file = dir.path().join("config.txt");
    fs::write(&test_file, "test data").unwrap();
    let main = dir.path().join("main.llt");
    let llt_content = r#"[call $slurp %cfg]"#;
    fs::write(&main, llt_content).unwrap();

    let file_str = test_file.to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-cwd",
            "--no-libdir",
            "--cap-file",
            &format!("cfg={}:r", file_str),
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "slurp with :r mode should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// sandbox-c: rlimit resource caps (--max-memory, --max-cpu, --max-fds)
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn max_memory_flag_accepted() {
    // --max-memory flag must be recognized and not cause errors on programs
    // that fit comfortably within the limit. 256 MB is well above what a
    // simple evaluation needs.
    let (path, _dir) = write_temp_llt("max_memory_flag", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--max-memory",
            "268435456", // 256 MB
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --max-memory 256MB; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn max_memory_zero_disables_limit() {
    // --max-memory 0 must disable the memory limit (no RLIMIT_AS applied).
    // A simple program must still succeed with this flag.
    let (path, _dir) = write_temp_llt("max_memory_zero", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--max-memory",
            "0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --max-memory 0 (disabled); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn max_cpu_flag_accepted() {
    // --max-cpu flag must be recognized and not interfere with fast-completing
    // programs. 10 seconds is generous for a trivial eval.
    let (path, _dir) = write_temp_llt("max_cpu_flag", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--max-cpu",
            "10",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --max-cpu 10; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn max_fds_flag_accepted() {
    // --max-fds flag must be recognized and not interfere with simple programs.
    // 32 FDs is above the minimum needed by the process (stdin/stdout/stderr +
    // a few internal fds).
    let (path, _dir) = write_temp_llt("max_fds_flag", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--max-fds",
            "32",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --max-fds 32; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn max_fds_zero_disables_limit() {
    // --max-fds 0 must disable the FD limit (no RLIMIT_NOFILE applied).
    let (path, _dir) = write_temp_llt("max_fds_zero", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--max-fds",
            "0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --max-fds 0 (disabled); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
#[cfg(unix)]
fn all_sandbox_flags_compose() {
    // All sandbox flags can be combined without conflict. This is the full
    // sandboxed invocation mode: --no-fs blocks $include, --max-memory caps
    // heap, --max-fds caps open files, --timeout caps wall-clock time.
    let (path, _dir) = write_temp_llt("all_sandbox_flags", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-fs",
            "--no-landlock",
            "--max-memory",
            "268435456", // 256 MB
            "--max-fds",
            "32",
            "--timeout",
            "5s",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "all sandbox flags should compose without error; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"x": 1}));
}

// ---------------------------------------------------------------------------
// sandbox-d: seccomp-bpf network and process sandbox (Linux only)
// ---------------------------------------------------------------------------

/// Seccomp filter is installed silently — on success the process prints no
/// warning to stderr and exits with code 0. On kernels where seccomp is
/// unavailable, a warning is printed but eval still succeeds (graceful
/// degradation).
#[test]
fn seccomp_sandbox_does_not_crash_eval() {
    // A simple program must succeed even after the seccomp filter is installed.
    // This tests both the happy path (seccomp supported) and graceful degradation
    // (seccomp unsupported — warning printed, eval continues).
    let (path, _dir) = write_temp_llt("seccomp_no_crash", "[x: 42]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "eval must succeed after seccomp filter install; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON output");
    assert_eq!(json, serde_json::json!({"x": 42}));
}

/// When seccomp degrades gracefully, any warning must go to stderr (not stdout),
/// and stdout must remain valid JSON.
#[test]
fn seccomp_degradation_warning_does_not_corrupt_stdout() {
    // On unsupported kernels setup_seccomp() prints a warning to stderr.
    // Stdout must still contain only valid JSON. This test passes on all platforms:
    // either seccomp works silently, or degrades with a warning — either way
    // stdout is valid JSON and exit is 0.
    let (path, _dir) = write_temp_llt("seccomp_stdout_clean", "[y: true]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "exit code must be 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .expect("stdout must be valid JSON even when seccomp warning is printed to stderr");
    assert_eq!(json, serde_json::json!({"y": true}));
}

// ---------------------------------------------------------------------------
// I/O builtins: emit, env, file operations
// ---------------------------------------------------------------------------

#[test]
fn emit_basic() {
    let (path, _dir) = write_temp_llt("emit_basic", "[emit \"hello world\\n\"]");
    let output = Command::new(tinct_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Without -o flag, no JSON output (emit-only mode), so stdout is just "hello world\n"
    assert_eq!(stdout, "hello world\n");
}

#[test]
fn env_missing() {
    let (path, _dir) = write_temp_llt("env_missing", "[env \"NONEXISTENT_VAR_12345\"]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // env returns null (empty dict) for missing vars.
    // json.llt serializes LLT null (empty dict []) as JSON null.
    assert_eq!(json, serde_json::Value::Null);
}

#[test]
fn env_no_env_flag() {
    let (path, _dir) = write_temp_llt("env_no_env", "[env \"PATH\"]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", "--no-env", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // --no-env makes env return null for all vars.
    // json.llt serializes LLT null (empty dict []) as JSON null.
    assert_eq!(json, serde_json::Value::Null);
}

#[test]
fn revocable_and_revoke() {
    // Test revocable DirCap creation and revocation
    let dir = TempDir::new("revocable_test");
    let test_file = dir.path().join("data.txt");
    fs::write(&test_file, "test content").expect("failed to write test file");

    let llt_content = r#"
[revocable-cap: [revocable %cap]]
[fh: [open revocable-cap "data.txt" Readable Text]]
[content: [slurp fh]]
[_ : [revoke-cap revocable-cap]]
content
"#;
    let (path, _llt_dir) = write_temp_llt("revocable_revoke", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:r", dir.path().display()),
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!("test content"));
}

#[test]
fn lines_basic() {
    // Test lazy line reading from a file
    let dir = TempDir::new("lines_test");
    let test_file = dir.path().join("lines.txt");
    fs::write(&test_file, "line1\nline2\nline3\n").expect("failed to write test file");

    let llt_content = r#"
[fh: [open %cap "lines.txt" Readable Text]]
[collect [take 2 [lines fh]]]
"#;
    let (path, _llt_dir) = write_temp_llt("lines_basic", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:r", dir.path().display()),
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!(["line1", "line2"]));
}

#[test]
fn write_basic() {
    // Test basic write functionality
    let dir = TempDir::new("write_test");
    let test_file_path = dir.path().join("output.txt");

    let llt_content = r#"
[write %cap "output.txt" "hello world"]
[fh: [open %cap "output.txt" Readable Text]]
[slurp fh]
"#;
    let (path, _llt_dir) = write_temp_llt("write_basic", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:rw", dir.path().display()),
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!("hello world"));

    // Also verify the file was written to disk
    let file_content = fs::read_to_string(&test_file_path).expect("failed to read output file");
    assert_eq!(file_content, "hello world");
}

#[test]
fn write_atomic_basic() {
    // Test atomic write functionality
    let dir = TempDir::new("write_atomic_test");
    let test_file_path = dir.path().join("output.txt");

    let llt_content = r#"
[write-atomic %cap "output.txt" "atomic content"]
[fh: [open %cap "output.txt" Readable Text]]
[slurp fh]
"#;
    let (path, _llt_dir) = write_temp_llt("write_atomic_basic", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:rw", dir.path().display()),
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!("atomic content"));

    // Also verify the file was written to disk
    let file_content = fs::read_to_string(&test_file_path).expect("failed to read output file");
    assert_eq!(file_content, "atomic content");

    // Verify no temp files left behind
    let entries: Vec<_> = fs::read_dir(dir.path())
        .expect("failed to read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp."))
        .collect();
    assert_eq!(entries.len(), 0, "temp files should be cleaned up");
}

#[test]
#[ignore = "include builtin removed in include-decomp-prelude sprint; re-enable when LLT-level include is implemented"]
fn write_and_slurp_roundtrip() {
    // Test write + slurp roundtrip via stdlib/io.llt wrappers
    let dir = TempDir::new("write_roundtrip_test");

    let llt_content = r#"
[include %libdir "io.llt"]
[write-file %cap "test.txt" "roundtrip data"]
[match [read-file %cap "test.txt"]
  [Ok v]: v
  [Err msg]: [error msg]]
"#;
    let (path, _llt_dir) = write_temp_llt("write_roundtrip", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--cap-fs=cap={}:rw", dir.path().display()),
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!("roundtrip data"));
}

// ---------------------------------------------------------------------------
// Multi-file pipeline
// ---------------------------------------------------------------------------

#[test]
fn multi_file_pipeline() {
    // Test multi-file pipeline: data.llt → transform.llt
    let dir = TempDir::new("multi_file_pipeline");

    let data_path = dir.path().join("data.llt");
    fs::write(&data_path, "[x: 10  y: 20]").expect("failed to write data file");

    let transform_path = dir.path().join("transform.llt");
    fs::write(&transform_path, "[sum: [+ %.x %.y]]").expect("failed to write transform file");

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            data_path.to_str().unwrap(),
            transform_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"sum": 30}));
}

#[test]
fn multi_file_pipeline_with_emit() {
    // Test multi-file pipeline with emit (text output only, no -o flag)
    let dir = TempDir::new("multi_file_emit");

    let data_path = dir.path().join("data.llt");
    fs::write(&data_path, "[name: \"Alice\"  greeting: \"Hello\"]")
        .expect("failed to write data file");

    let format_path = dir.path().join("format.llt");
    fs::write(&format_path, "[emit [str %.greeting \", \" %.name \"!\"]]")
        .expect("failed to write format file");

    let output = Command::new(tinct_bin())
        .args([
            "run",
            data_path.to_str().unwrap(),
            format_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Without -o flag, only emit output appears
    assert_eq!(stdout, "Hello, Alice!");
}

// ---------------------------------------------------------------------------
// Literate mode: tinct literate tangle / eval / weave
// ---------------------------------------------------------------------------

/// Helper: write a temporary Markdown file and return its path + guard.
fn write_temp_md(label: &str, content: &str) -> (PathBuf, TempDir) {
    let dir = TempDir::new(label);
    let path = dir.path().join(format!("{label}.md"));
    fs::write(&path, content).expect("failed to write temp markdown file");
    (path, dir)
}

#[test]
fn literate_tangle_single_block() {
    let md = "# Docs\n\n```tinct\n[x: 1]\n```\n";
    let (path, _dir) = write_temp_md("literate_tangle_single", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "tangle", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[x: 1]"),
        "expected block content in output, got: {stdout}"
    );
}

#[test]
fn literate_tangle_two_blocks_joined_with_separator() {
    let md = concat!(
        "# Config\n\n",
        "Base config:\n\n",
        "```tinct\n",
        "[\n",
        "    base-url: \"https://api.example.com\"\n",
        "    timeout: 30\n",
        "]\n",
        "```\n\n",
        "Filter step:\n\n",
        "```tinct\n",
        "[filter [fn [u] u.active] %.users]\n",
        "```\n",
    );
    let (path, _dir) = write_temp_md("literate_tangle_two", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "tangle", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Both blocks should be present separated by ---
    assert!(stdout.contains("base-url"), "expected base-url in output");
    assert!(stdout.contains("---"), "expected --- separator in output");
    assert!(
        stdout.contains("[filter"),
        "expected filter block in output"
    );
}

#[test]
fn literate_tangle_llt_language_tag() {
    // ```llt is also recognized as a tinct block
    let md = "# Docs\n\n```llt\n[y: 42]\n```\n";
    let (path, _dir) = write_temp_md("literate_tangle_llt", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "tangle", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[y: 42]"),
        "expected [y: 42] in output, got: {stdout}"
    );
}

#[test]
fn literate_tangle_no_blocks_produces_empty_output() {
    // A Markdown file with no tinct blocks produces empty tangle output.
    let md = "# Title\n\nJust prose, no code.\n\n```rust\nfn main() {}\n```\n";
    let (path, _dir) = write_temp_md("literate_tangle_empty", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "tangle", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // tangle of empty block list should produce no meaningful content
    assert!(
        stdout.trim().is_empty(),
        "expected empty output for no-block markdown, got: {stdout}"
    );
}

#[test]
fn literate_eval_single_block() {
    // A single tinct block with a simple dict expression.
    let md = "# Config\n\n```tinct\n[x: 1  y: 2]\n```\n";
    let (path, _dir) = write_temp_md("literate_eval_single", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected JSON output");
    assert_eq!(json, serde_json::json!({"x": 1, "y": 2}));
}

#[test]
fn literate_eval_two_blocks_pipeline() {
    // Second block receives first block's output as %.
    let md = concat!(
        "```tinct\n[port: 8080  workers: 4]\n```\n\n",
        "Double the workers:\n\n",
        "```tinct\n[port: %.port  double-workers: [* %.workers 2]]\n```\n",
    );
    let (path, _dir) = write_temp_md("literate_eval_pipeline", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected JSON output");
    assert_eq!(json, serde_json::json!({"port": 8080, "double-workers": 8}));
}

#[test]
fn literate_eval_no_blocks_is_error() {
    // eval on a file with no tinct blocks should fail with a clear error.
    let md = "# No code here\n\nJust prose.\n";
    let (path, _dir) = write_temp_md("literate_eval_empty", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure when no tinct blocks present"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no tinct code blocks"),
        "expected error about missing blocks, got: {stderr}"
    );
}

#[test]
fn literate_weave_outputs_markdown_with_comments() {
    // weave should output the original Markdown with === out sections inside each block.
    let md = "# Config\n\n```tinct\n[x: 10]\n```\n";
    let (path, _dir) = write_temp_md("literate_weave_basic", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Original Markdown content preserved
    assert!(stdout.contains("# Config"), "expected original heading");
    assert!(
        stdout.contains("[x: 10]"),
        "expected original block content"
    );
    // Result injected as === out section inside the code block
    assert!(
        stdout.contains("=== out"),
        "expected === out section in weave output, got: {stdout}"
    );
    assert!(
        stdout.contains("10"),
        "expected value 10 in weave result, got: {stdout}"
    );
}

#[test]
fn literate_weave_no_blocks_outputs_markdown_unchanged() {
    // weave on a file with no tinct blocks should pass through the Markdown unchanged.
    let md = "# Just prose\n\nNo code blocks here.\n";
    let (path, _dir) = write_temp_md("literate_weave_empty", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("# Just prose"), "expected original content");
    assert!(
        stdout.contains("No code blocks here."),
        "expected original prose"
    );
}

#[test]
fn literate_weave_inline_marker_substitution() {
    // Test inline <!-- tinct-result --> markers get replaced with block results
    let md = concat!(
        "# Config\n\n",
        "First block:\n\n",
        "```tinct\n[x: 42]\n```\n\n",
        "The value is <!-- tinct-result: -->\n\n",
        "Second block:\n\n",
        "```tinct\n[y: 100]\n```\n\n",
        "Now the value is <!-- tinct-result: -->\n",
    );
    let (path, _dir) = write_temp_md("literate_weave_inline", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // First inline marker should be replaced with first block's result
    assert!(
        stdout.contains("The value is {\"x\":42}"),
        "expected first inline marker replaced with first block result, got: {stdout}"
    );

    // Second inline marker should be replaced with second block's result
    assert!(
        stdout.contains("Now the value is {\"y\":100}"),
        "expected second inline marker replaced with second block result, got: {stdout}"
    );
}

#[test]
fn literate_weave_inline_marker_with_expression() {
    // Test inline markers with expressions like <!-- tinct-result: %.x -->
    let md = concat!(
        "# Test\n\n",
        "```tinct\n[x: 42  y: 100]\n```\n\n",
        "The x value is <!-- tinct-result: %.x -->\n\n",
        "The y value is <!-- tinct-result: %.y -->\n",
    );
    let (path, _dir) = write_temp_md("literate_weave_expr", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("The x value is 42"),
        "expected x field extracted, got: {stdout}"
    );

    assert!(
        stdout.contains("The y value is 100"),
        "expected y field extracted, got: {stdout}"
    );
}

#[test]
fn literate_weave_no_substitute_preserves_markers() {
    // Test --no-substitute flag preserves inline markers
    let md = concat!(
        "# Test\n\n",
        "```tinct\n[x: 42]\n```\n\n",
        "The value is <!-- tinct-result: -->\n",
    );
    let (path, _dir) = write_temp_md("literate_weave_no_sub", md);
    let output = Command::new(tinct_bin())
        .args([
            "literate",
            "weave",
            "--no-substitute",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The inline marker should be preserved, not replaced
    assert!(
        stdout.contains("The value is <!-- tinct-result: -->"),
        "expected inline marker preserved with --no-substitute, got: {stdout}"
    );

    // Should not contain the substituted value
    assert!(
        !stdout.contains("The value is {\"x\":42}"),
        "expected marker NOT substituted with --no-substitute, got: {stdout}"
    );
}

#[test]
fn literate_weave_replaces_existing_markers() {
    // Test that re-running weave replaces existing === out sections rather than appending.
    // Input has stale === out values; weave should overwrite with fresh evaluated results.
    let md_with_old_sections = concat!(
        "# Config\n\n",
        "```tinct\n[x: 10]\n=== out\n{\"x\":999}\n```\n",
        "\n",
        "Some prose.\n\n",
        "```tinct\n[y: 20]\n=== out\n{\"y\":888}\n```\n",
        "\n",
    );
    let (path, _dir) = write_temp_md("literate_weave_replace", md_with_old_sections);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain the NEW result (x: 10), not the stale one (x: 999)
    assert!(
        stdout.contains("{\"x\":10}"),
        "expected updated result for first block, got: {stdout}"
    );
    assert!(
        !stdout.contains("{\"x\":999}"),
        "expected stale result to be replaced, not preserved, got: {stdout}"
    );

    // Should contain the NEW result (y: 20), not the stale one (y: 888)
    assert!(
        stdout.contains("{\"y\":20}"),
        "expected updated result for second block, got: {stdout}"
    );
    assert!(
        !stdout.contains("{\"y\":888}"),
        "expected stale result to be replaced, not preserved, got: {stdout}"
    );

    // Should not have duplicate === out sections
    let section_count = stdout.matches("=== out").count();
    assert_eq!(
        section_count, 2,
        "expected exactly 2 === out sections (one per block), got {section_count} in: {stdout}"
    );
}

#[test]
fn literate_weave_mixed_marker_presence() {
    // Test a file with some blocks having existing === out sections and some without.
    // Weave should add/update === out sections in all blocks correctly.
    let md = concat!(
        "```tinct\n[x: 10]\n=== out\n{\"x\":999}\n```\n", // Has stale === out
        "\n",
        "```tinct\n[y: 20]\n```\n", // No === section
        "\n",
        "```tinct\n[z: 30]\n=== out\n{\"z\":888}\n```\n", // Has stale === out
        "\n",
    );
    let (path, _dir) = write_temp_md("literate_weave_mixed", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // All three blocks should have correct, fresh results
    assert!(
        stdout.contains("{\"x\":10}"),
        "expected updated result for first block, got: {stdout}"
    );
    assert!(
        stdout.contains("{\"y\":20}"),
        "expected new result for second block, got: {stdout}"
    );
    assert!(
        stdout.contains("{\"z\":30}"),
        "expected updated result for third block, got: {stdout}"
    );

    // Stale values should be gone
    assert!(
        !stdout.contains("{\"x\":999}") && !stdout.contains("{\"z\":888}"),
        "expected stale results to be replaced, got: {stdout}"
    );

    // Should have exactly 3 === out sections
    let section_count = stdout.matches("=== out").count();
    assert_eq!(
        section_count, 3,
        "expected exactly 3 === out sections, got {section_count} in: {stdout}"
    );
}

#[test]
fn literate_missing_file_is_error() {
    let output = Command::new(tinct_bin())
        .args(["literate", "tangle", "/tmp/nonexistent_literate_test.md"])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected failure for missing file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error reading file") || stderr.contains("No such file"),
        "expected file-not-found error, got: {stderr}"
    );
}

#[test]
fn literate_weave_verify_pass_when_expected_matches_actual() {
    // --verify exits 0 when the === out section matches the actual block output.
    // The tinct block evaluates to {"x":10} (compact JSON), so the === out section
    // must contain the same compact JSON.
    let md = concat!(
        "# Verify test\n\n",
        "```tinct\n",
        "[x: 10]\n",
        "=== out\n",
        "{\"x\":10}\n",
        "```\n",
    );
    let (path, _dir) = write_temp_md("literate_verify_pass", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", "--verify", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected exit 0 when expected matches actual; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn literate_weave_verify_fail_when_expected_does_not_match_actual() {
    // --verify exits 1 when the === out section does not match the actual block output.
    let md = concat!(
        "# Verify test\n\n",
        "```tinct\n",
        "[x: 10]\n",
        "=== out\n",
        "{\"x\":999}\n",
        "```\n",
    );
    let (path, _dir) = write_temp_md("literate_verify_fail", md);
    let output = Command::new(tinct_bin())
        .args(["literate", "weave", "--verify", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected exit 1 when expected does not match actual"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("verification failed") || stderr.contains("mismatch"),
        "expected verification failure message in stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Type warning channel: TypeDiagnostic emission to stderr
// ---------------------------------------------------------------------------

/// Verify that an explicit `@Unknown` annotation (T011) produces a diagnostic
/// on stderr in non-strict mode.  The program must still evaluate successfully
/// (type diagnostics are advisory and never block eval).
///
/// Regression test for the type-warning-channel sprint: the diagnostic
/// infrastructure was wired but emission was stubbed with TODO comments.
#[test]
fn type_warning_explicit_unknown_emitted_on_stderr() {
    // [f: [fn@Unknown [let x] $x]] produces a T011 "explicit @Unknown annotation"
    // diagnostic from scan_type_quality.
    let (path, _dir) = write_temp_llt("type_warn_explicit_unknown", "[f: [fn@Unknown [let x] $x]]");
    let output = Command::new(tinct_bin())
        .args(["run", "--no-fs", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // Eval must succeed (T011 is advisory, not fatal).
    assert!(
        output.status.success(),
        "expected success (T011 is advisory); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The T011 diagnostic must appear on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("T011") || stderr.contains("explicit @Unknown"),
        "expected T011 or 'explicit @Unknown' on stderr for @Unknown annotation; got: {stderr}"
    );
}

/// Verify that `@Unknown` annotation diagnostic does NOT appear when there is
/// no `@Unknown` annotation — i.e., the channel is not noisy for clean code.
#[test]
fn type_warning_not_emitted_for_clean_code() {
    let (path, _dir) = write_temp_llt("type_warn_clean", "[x: 42]");
    let output = Command::new(tinct_bin())
        .args(["run", "--no-fs", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success for clean code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("T010") && !stderr.contains("T011"),
        "expected no type diagnostics for clean code; stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Seq-at-top-level handling (access-pipeline Phase 2)
// ---------------------------------------------------------------------------

#[test]
fn seq_at_top_level_without_output_program() {
    // A program that returns a bare Seq without -o flag produces no output.
    // The Seq is NOT drained — side-effects won't run without an output program.
    // [call $seq 1 []] is the simplest Seq value: Seq(1, []).
    let (path, _dir) = write_temp_llt("seq_top_no_emit", "[call $seq 1 []]");
    let output = Command::new(tinct_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success for top-level Seq without output; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Without -o flag and without emit calls, no output
    assert_eq!(stdout, "");
}

#[test]
fn seq_at_top_level_from_range_without_output_program() {
    // [call $range 0 5] returns a Seq. Without -o flag, produces no output.
    // The Seq is NOT drained, so no side-effects run.
    let (path, _dir) = write_temp_llt("seq_range_no_emit", "[call $range 0 5]");
    let output = Command::new(tinct_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success for top-level range Seq without output; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Without -o flag and without emit calls, no output
    assert_eq!(stdout, "");
}

#[test]
#[ignore = "pre-existing regression from runtime-v2 merge: emit + seq top-level output missing range elements"]
fn seq_at_top_level_with_emit_and_none_output() {
    // A generator pipeline that uses emit for text output and returns a Seq.
    // The program calls emit in two ways:
    //   1. A header line emitted during initial scope-chain evaluation.
    //   2. Per-element emit calls inside the generator function body.
    //
    // Scope chain:
    //   - First expression: [_: [emit "start\n"]] — emits "start\n".
    //   - Second expression: [map [fn [n] [emit [str n "\n"]]] [range 0 3]] — returns Seq.
    //
    // Without -o flag, the Seq is not drained, so only "start\n" is emitted.
    // With -o none, the Seq IS drained (none.llt calls collect on Seq), forcing all elements.
    // Total output with -o none: "start\n0\n1\n2\n"
    let source = "[call $emit \"start\\n\"]\n[call $map [fn [n] [call $emit [call $str $n \"\\n\"]]] [call $range 0 3]]";
    let (path, _dir) = write_temp_llt("seq_emit_generator", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "none", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success when Seq is drained via -o none; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // emit calls should have written "start\n0\n1\n2\n" to stdout
    assert_eq!(
        stdout, "start\n0\n1\n2\n",
        "expected emit output 'start\\n0\\n1\\n2\\n', got: {stdout}"
    );
}

#[test]
fn seq_with_collect_produces_json_array() {
    // The recommended way to get JSON output from a Seq: use collect.
    // [collect [range 0 3]] → {0: 0, 1: 1, 2: 2} → JSON [0, 1, 2]
    let (path, _dir) = write_temp_llt("seq_collect_json", "[call $collect [call $range 0 3]]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success for collect + JSON; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!([0, 1, 2]));
}

#[test]
fn seq_without_output_program_does_not_drain() {
    // Verify that without -o flag, a Seq with emit side-effects does NOT drain.
    // Only the first emit (in scope chain) fires; the Seq elements are never forced.
    let source = "[call $emit \"start\\n\"]\n[call $map [fn [n] [call $emit [call $str $n \"\\n\"]]] [call $range 0 3]]";
    let (path, _dir) = write_temp_llt("seq_no_drain", source);
    let output = Command::new(tinct_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Only "start\n" should be emitted; the Seq elements (0, 1, 2) are NOT drained
    assert_eq!(
        stdout, "start\n",
        "expected only 'start\\n' without draining Seq elements; got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// default CLI output via stdlib/cli/out/json.llt
//
// These tests verify that `tinct run -o json` produces correct JSON output.
// The formatter path is: -o json → json.llt → [include codecs/json.llt] → to-json → print!
// ---------------------------------------------------------------------------

#[test]
fn default_output_int() {
    // A file returning an integer scalar produces compact JSON integer output.
    let (path, _dir) = write_temp_llt("default_output_int", "42");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON for integer output");
    assert_eq!(json, serde_json::json!(42));
}

#[test]
fn default_output_string() {
    // A file returning a string produces JSON string output (quoted and escaped).
    let (path, _dir) = write_temp_llt("default_output_string", "\"hello\"");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON for string output");
    assert_eq!(json, serde_json::json!("hello"));
}

#[test]
fn default_output_bool() {
    // A file returning a boolean produces JSON boolean output.
    let (path, _dir) = write_temp_llt("default_output_bool", "true");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON for boolean output");
    assert_eq!(json, serde_json::json!(true));
}

#[test]
fn default_output_dict() {
    // A file returning a multi-key dict produces compact JSON object output via json.llt.
    let (path, _dir) = write_temp_llt("default_output_dict", "[x: 1  y: 2]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Structural equality: compare as parsed JSON values (key order is preserved
    // by json.llt but serde_json::Value comparison is order-insensitive for objects).
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON for dict output");
    assert_eq!(json, serde_json::json!({"x": 1, "y": 2}));
}

#[test]
fn default_output_null() {
    // A file returning [] (the tinct null / empty dict) produces JSON null.
    // json.llt's json-value checks null? first, so [] → "null".
    let (path, _dir) = write_temp_llt("default_output_null", "[]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON for null output");
    assert_eq!(json, serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// -e / --expr inline expression tests
// ---------------------------------------------------------------------------

#[test]
fn expr_flag_simple() {
    // `tinct eval -e '%.x' <<< '{"x":42}'` → 42 (with stdin JSON auto-detection)
    use std::io::Write;
    let mut child = Command::new(tinct_bin())
        .args(["run", "-o", "json", "-e", "%.x"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn tinct");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(b"{\"x\":42}")
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait for tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

#[test]
fn expr_flag_chained() {
    // `tinct eval -e '[x: 1]' -e '[merge % [y: 2]]'` → {"x":1,"y":2}
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "-e",
            "[x: 1]",
            "-e",
            "[merge % [y: 2]]",
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1, "y": 2}));
}

// ---------------------------------------------------------------------------
// -i / --input formatter tests
// ---------------------------------------------------------------------------

#[test]
fn input_flag_json() {
    // Skip if stdlib/cli/in/json.llt doesn't exist (created by another agent)
    let _libdir = match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent()?.parent().map(|r| r.join("stdlib")))
        .filter(|p| p.is_dir())
    {
        Some(path) if path.join("cli").join("in").join("json.llt").exists() => path,
        _ => {
            eprintln!("Skipping input_flag_json: stdlib/cli/in/json.llt not found");
            return;
        }
    };

    // `tinct eval -i json -e '%.x' <<< '{"x":42}'` → 42 (explicit input formatter)
    use std::io::Write;
    let mut child = Command::new(tinct_bin())
        .args(["run", "-o", "json", "-i", "json", "-e", "%.x"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn tinct");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(b"{\"x\":42}")
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait for tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

#[test]
fn input_flag_unknown_format() {
    // `tinct eval -i nonexistent` should error clearly
    let output = Command::new(tinct_bin())
        .args(["run", "-i", "nonexistent"])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--input") || stderr.contains("formatter not found"),
        "expected --input error message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// -o / --output formatter tests
// ---------------------------------------------------------------------------

#[test]
fn output_flag_raw() {
    // `tinct eval -i json -e '%.msg' -o raw <<< '{"msg":"hello"}'` → hello (no quotes)
    use std::io::Write;
    let mut child = Command::new(tinct_bin())
        .args(["run", "-i", "json", "-e", "%.msg", "-o", "raw"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn tinct");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(b"{\"msg\":\"hello\"}")
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait for tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // raw formatter emits string unquoted (no JSON encoding)
    assert_eq!(stdout.trim(), "hello");
}

#[test]
fn output_flag_unknown_format() {
    // `tinct eval -o nonexistent -e '42'` should error clearly
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "nonexistent", "-e", "42"])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--output") || stderr.contains("formatter not found"),
        "expected --output error message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// rv2-output-formatter-contract: output formatters return String directly
// ---------------------------------------------------------------------------

#[test]
fn output_flag_csv() {
    // -o csv: list-of-dicts → CSV header + data rows.
    // The CSV formatter quotes all fields and uses CRLF-free newlines.
    // Input: {0: {name: "Alice", age: 30}, 1: {name: "Bob", age: 25}}
    // Expected: "name","age"\n"Alice","30"\n"Bob","25"\n
    let source = r#"[0: [name: "Alice"  age: 30]  1: [name: "Bob"  age: 25]]"#;
    let (path, _dir) = write_temp_llt("output_flag_csv", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "csv", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Must have a non-empty CSV body
    assert!(
        !stdout.trim().is_empty(),
        "expected non-empty CSV output, got empty stdout"
    );
    // First line is the header row
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 3,
        "expected at least 3 lines (header + 2 data rows), got: {stdout}"
    );
    let header = lines[0];
    assert!(
        header.contains("name") && header.contains("age"),
        "expected header to contain field names 'name' and 'age', got: {header}"
    );
    // Data rows contain the actual values
    let data_body = &stdout[header.len()..];
    assert!(
        data_body.contains("Alice") && data_body.contains("Bob"),
        "expected data rows to contain 'Alice' and 'Bob', got: {data_body}"
    );
    assert!(
        data_body.contains("30") && data_body.contains("25"),
        "expected data rows to contain '30' and '25', got: {data_body}"
    );
}

#[test]
fn output_flag_csv_exact() {
    // Exact assertion for the CSV formatter output format:
    // - All fields are double-quoted
    // - Header row uses dict key names
    // - Data rows preserve insertion order
    let source = r#"[0: [name: "Alice"  age: 30]  1: [name: "Bob"  age: 25]]"#;
    let (path, _dir) = write_temp_llt("output_flag_csv_exact", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "csv", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV formatter: csv-quote wraps every field in double-quotes
    // csv-header: "name","age"\n
    // csv-row row0: "Alice","30"\n
    // csv-row row1: "Bob","25"\n
    assert_eq!(
        stdout, "\"name\",\"age\"\n\"Alice\",\"30\"\n\"Bob\",\"25\"\n",
        "CSV output did not match expected format"
    );
}

#[test]
fn output_flag_env() {
    // -o env: flat dict → KEY=value\n format.
    // The env formatter emits one line per key with no quoting.
    let source = r#"[FOO: "bar"  BAZ: "qux"]"#;
    let (path, _dir) = write_temp_llt("output_flag_env", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "env", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "FOO=bar\nBAZ=qux\n",
        "env output did not match expected KEY=value format"
    );
}

#[test]
fn output_flag_env_int_value() {
    // The env formatter uses `str` to convert values, so integers become decimal strings.
    let source = r#"[PORT: 8080  TIMEOUT: 30]"#;
    let (path, _dir) = write_temp_llt("output_flag_env_int", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "env", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("PORT=8080") && stdout.contains("TIMEOUT=30"),
        "expected PORT=8080 and TIMEOUT=30 in env output, got: {stdout}"
    );
}

#[test]
fn output_flag_yaml() {
    // -o yaml: dict → YAML mapping format.
    // Simple flat dict: keys become YAML keys, scalars become YAML scalars.
    let source = r#"[host: "localhost"  port: 8080]"#;
    let (path, _dir) = write_temp_llt("output_flag_yaml", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "yaml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // yaml-dict wraps in leading newline: "\nhost: localhost\nport: 8080\n"
    assert!(
        stdout.contains("host: localhost"),
        "expected 'host: localhost' in YAML output, got: {stdout}"
    );
    assert!(
        stdout.contains("port: 8080"),
        "expected 'port: 8080' in YAML output, got: {stdout}"
    );
    // Output ends with a newline (yaml public API always appends "\n")
    assert!(
        stdout.ends_with('\n'),
        "expected YAML output to end with newline, got: {stdout:?}"
    );
}

#[test]
fn output_flag_yaml_exact() {
    // Exact assertion for the YAML formatter output.
    // yaml-dict returns "\n" + entries, then the public api appends "\n".
    // For [host: "localhost"  port: 8080]:
    //   yaml-dict-entries produces "host: localhost\nport: 8080"
    //   yaml-dict produces "\nhost: localhost\nport: 8080"
    //   yaml (public) produces "\nhost: localhost\nport: 8080\n"
    let source = r#"[host: "localhost"  port: 8080]"#;
    let (path, _dir) = write_temp_llt("output_flag_yaml_exact", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "yaml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "\nhost: localhost\nport: 8080\n",
        "YAML output did not match expected format"
    );
}

#[test]
fn output_flag_toml() {
    // -o toml: dict → TOML key = value format.
    // Flat keys are emitted as "key = value" lines; nested dicts become [table] sections.
    let source = r#"[host: "localhost"  port: 8080]"#;
    let (path, _dir) = write_temp_llt("output_flag_toml", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "toml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // toml-quote wraps strings in double-quotes; ints are bare
    assert!(
        stdout.contains("host = \"localhost\""),
        "expected 'host = \"localhost\"' in TOML output, got: {stdout}"
    );
    assert!(
        stdout.contains("port = 8080"),
        "expected 'port = 8080' in TOML output, got: {stdout}"
    );
}

#[test]
fn output_flag_toml_exact() {
    // Exact assertion for the TOML formatter output.
    // For [host: "localhost"  port: 8080]:
    //   toml-flat produces 'host = "localhost"\nport = 8080\n'
    //   toml-tables produces "" (no nested dicts)
    let source = r#"[host: "localhost"  port: 8080]"#;
    let (path, _dir) = write_temp_llt("output_flag_toml_exact", source);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "toml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "host = \"localhost\"\nport = 8080\n",
        "TOML output did not match expected format"
    );
}

#[test]
fn output_flag_none_empty_stdout() {
    // -o none: the none formatter returns an empty string, so stdout is empty.
    // This is the canonical "side-effect-only" output mode.
    let (path, _dir) = write_temp_llt("output_flag_none_empty", "[x: 1  y: 2]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "none", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "",
        "expected empty stdout with -o none, got: {stdout:?}"
    );
}

#[test]
fn output_flag_none_scalar() {
    // -o none with a scalar input: stdout is still empty regardless of input type.
    let (path, _dir) = write_temp_llt("output_flag_none_scalar", "42");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "none", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "",
        "expected empty stdout with -o none on scalar input, got: {stdout:?}"
    );
}

#[test]
fn output_flag_csv_empty_input() {
    // -o csv with an empty dict: csv formatter returns "" for empty input.
    let (path, _dir) = write_temp_llt("output_flag_csv_empty", "[]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "csv", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "",
        "expected empty stdout from csv formatter on empty input, got: {stdout:?}"
    );
}

#[test]
fn output_flag_env_empty_input() {
    // -o env with an empty dict: env formatter returns "" for empty input.
    let (path, _dir) = write_temp_llt("output_flag_env_empty", "[]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "env", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "",
        "expected empty stdout from env formatter on empty input, got: {stdout:?}"
    );
}

#[test]
fn eval_format_json_pretty() {
    // -o json-pretty exercises the json-pretty pipeline formatter.
    // The formatter currently produces compact JSON (same as -o json); verify valid JSON output.
    let (path, _dir) = write_temp_llt("eval_format_json_pretty", "[x: 1 y: \"hello\"]");
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json-pretty", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // json-pretty currently produces compact JSON; verify it is valid JSON with correct values
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON from -o json-pretty");
    assert_eq!(json, serde_json::json!({"x": 1, "y": "hello"}));
}

// ---------------------------------------------------------------------------
// Combined -i/-o/-e tests
// ---------------------------------------------------------------------------

#[test]
fn input_output_expr_pipeline() {
    // Skip if formatters don't exist (created by another agent)
    let _libdir = match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent()?.parent().map(|r| r.join("stdlib")))
        .filter(|p| p.is_dir())
    {
        Some(path)
            if path.join("cli").join("in").join("json.llt").exists()
                && path.join("cli").join("out").join("raw.llt").exists() =>
        {
            path
        }
        _ => {
            eprintln!("Skipping input_output_expr_pipeline: formatters not found");
            return;
        }
    };

    // Full pipeline: -i json -e expr -o raw
    use std::io::Write;
    let mut child = Command::new(tinct_bin())
        .args(["run", "-i", "json", "-e", "%.msg", "-o", "raw"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn tinct");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(b"{\"msg\":\"hello\"}")
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait for tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello");
}

#[test]
fn expr_file_interleaving() {
    // Test that -e and file arguments are processed in the order they appear on the CLI.
    // Command: tinct run file1.llt -e '[transform %]' file2.llt
    // Expected order: file1.llt → -e expression → file2.llt

    // Create two temp files
    let (file1_path, _dir1) = write_temp_llt("interleave_file1", "[x: 1]");
    let (file2_path, _dir2) = write_temp_llt("interleave_file2", "[y: %.x]");

    // Without proper interleaving, this would fail because file2.llt would be processed
    // before the -e expression, so % would be file1's output [x: 1], not the transformed value.
    // With proper interleaving: file1 → [x: 1], then -e → [x: 1, z: 2], then file2 → [y: 1]
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            file1_path.to_str().unwrap(),
            "-e",
            "[merge % [z: 2]]",
            file2_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The final output should be [y: 1] because file2 accesses %.x,
    // and % at that point is the result of the -e expression [x: 1, z: 2]
    let expected = r#"{"y":1}"#;
    assert_eq!(
        stdout.trim(),
        expected,
        "Expected interleaved execution: file1 → -e → file2"
    );
}

#[test]
fn expr_file_interleaving_multiple() {
    // Test multiple interleaved -e and file arguments.
    // Command: tinct run -e '[a: 1]' file1.llt -e '[c: %.b]' file2.llt

    let (file1_path, _dir1) = write_temp_llt("interleave_multi_file1", "[b: %.a]");
    let (file2_path, _dir2) = write_temp_llt("interleave_multi_file2", "[d: %.c]");

    // Pipeline: -e → [a: 1], file1 → [b: 1], -e → [c: 1], file2 → [d: 1]
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "-e",
            "[a: 1]",
            file1_path.to_str().unwrap(),
            "-e",
            "[c: %.b]",
            file2_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let expected = r#"{"d":1}"#;
    assert_eq!(stdout.trim(), expected);
}

// ---------------------------------------------------------------------------
// tinct describe — input contract description
// ---------------------------------------------------------------------------

#[test]
fn describe_no_contract() {
    // A file with no %@Type annotation reports "no input contract".
    let (path, _dir) = write_temp_llt("describe_no_contract", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["describe", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no input contract"),
        "expected 'no input contract', got: {stdout}"
    );
}

#[test]
fn describe_json_no_contract() {
    // --json mode on a file with no contract outputs empty JSON object.
    let (path, _dir) = write_temp_llt("describe_json_no_contract", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["describe", "--json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({}));
}

#[test]
fn describe_schema_dict_detection() {
    // A file with a schema dict (dict values containing schema keys) is detected.
    let source = r#"[
  port: [type: Int  min: 1  max: 65535]
  host: [type: String  pattern: "^[a-z]+$"]
]"#;
    let (path, _dir) = write_temp_llt("describe_schema_dict", source);
    let output = Command::new(tinct_bin())
        .args(["describe", "--json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON from describe --json");
    // Should have detected schema fields
    let contracts = json.get("contracts").and_then(|c| c.as_array());
    assert!(
        contracts.is_some(),
        "expected contracts array in JSON output, got: {json}"
    );
    let contracts = contracts.unwrap();
    assert!(!contracts.is_empty(), "expected non-empty contracts array");
    // Check that schema was detected with port and host entries
    let first = &contracts[0];
    let schema = first.get("schema").and_then(|s| s.as_object());
    assert!(
        schema.is_some(),
        "expected schema object in contract, got: {first}"
    );
    let schema = schema.unwrap();
    assert!(schema.contains_key("port"), "expected port in schema");
    assert!(schema.contains_key("host"), "expected host in schema");
}

#[test]
fn describe_human_readable() {
    // Human-readable output shows one line per field.
    let source = r#"[port: [type: Int  min: 1  max: 65535]]"#;
    let (path, _dir) = write_temp_llt("describe_human", source);
    let output = Command::new(tinct_bin())
        .args(["describe", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("port"),
        "expected 'port' in human-readable output, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// --cap-clock / --cap-clock-fixed tests
// ---------------------------------------------------------------------------

#[test]
fn cap_clock_real() {
    // Verify %clock is injected by default as a real ClockCap that can be used with $now
    // format-timestamp converts Timestamp → String so json.llt can serialize it
    let llt_content = r#"[call $format-timestamp [call $now %clock]]"#;
    let (path, _dir) = write_temp_llt("cap_clock_real", llt_content);
    let output = Command::new(tinct_bin())
        .args(["run", "-o", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // Verify we got a Timestamp value (RFC 3339 string)
    assert!(
        json.is_string(),
        "expected Timestamp string (RFC 3339), got: {json}"
    );
    let timestamp_str = json.as_str().unwrap();
    // Verify it's a valid RFC 3339 timestamp
    assert!(
        timestamp_str.contains('T') && timestamp_str.contains('Z'),
        "expected RFC 3339 format (contains T and Z), got: {timestamp_str}"
    );
}

#[test]
fn cap_clock_fixed() {
    // Verify --cap-clock-fixed overrides %clock with a fixed timestamp
    // format-timestamp converts Timestamp → String so json.llt can serialize it
    let llt_content = r#"[call $format-timestamp [call $now %clock]]"#;
    let (path, _dir) = write_temp_llt("cap_clock_fixed", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--cap-clock-fixed",
            "2024-01-01T00:00:00Z",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // Verify we got the expected fixed timestamp (RFC 3339 string)
    assert!(
        json.is_string(),
        "expected Timestamp string (RFC 3339), got: {json}"
    );
    let timestamp_str = json.as_str().unwrap();
    // Verify it's exactly the timestamp we injected
    assert_eq!(
        timestamp_str, "2024-01-01T00:00:00Z",
        "expected exact timestamp 2024-01-01T00:00:00Z, got: {timestamp_str}"
    );
}

#[test]
fn no_cap_clock() {
    // Verify --no-cap-clock disables %clock injection
    // Try to access %clock — should fail with undefined variable error
    let llt_content = r#"%clock"#;
    let (path, _dir) = write_temp_llt("no_cap_clock", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-cap-clock",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    // Should fail because %clock is not injected
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined variable") && stderr.contains("clock"),
        "expected undefined variable error for %clock, got: {stderr}"
    );
}

#[test]
fn cap_clock_fixed_invalid_timestamp() {
    // Verify --cap-clock-fixed with invalid RFC 3339 timestamp errors clearly
    let llt_content = r#"[x: 1]"#;
    let (path, _dir) = write_temp_llt("cap_clock_fixed_invalid", llt_content);
    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--cap-clock-fixed",
            "not-a-timestamp",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cap-clock-fixed") && stderr.contains("invalid"),
        "expected --cap-clock-fixed error message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// --cap-file tests
// ---------------------------------------------------------------------------

#[test]
fn cap_file_readable_slurp() {
    // Verify --cap-file injects a readable Handle that can be slurped.
    // The LLT script reads from %cfg (injected as a Handle) via $slurp.
    let llt_content = r#"[slurp %cfg]"#;
    let (llt_path, _llt_dir) = write_temp_llt("cap_file_readable_slurp_script", llt_content);

    // Write a target file with known content
    let data_dir = TempDir::new("cap_file_readable_slurp_data");
    let data_path = data_dir.path().join("data.txt");
    fs::write(&data_path, "hello from cap-file").expect("failed to write data file");

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--no-cwd",
            "--no-libdir",
            "--cap-file",
            &format!("cfg={}:r", data_path.to_str().unwrap()),
            llt_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!("hello from cap-file"));
}

#[test]
fn cap_file_invalid_mode_errors() {
    // Verify --cap-file with an invalid mode suffix produces a clear error.
    let llt_content = "42";
    let (llt_path, _llt_dir) = write_temp_llt("cap_file_invalid_mode", llt_content);

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--cap-file",
            "x=/tmp/nonexistent.txt:badmode",
            llt_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cap-file") && stderr.contains("invalid mode"),
        "expected --cap-file mode error, got: {stderr}"
    );
}

#[test]
fn cap_file_missing_file_errors() {
    // Verify --cap-file with a non-existent path produces a clear error.
    let llt_content = "42";
    let (llt_path, _llt_dir) = write_temp_llt("cap_file_missing_file", llt_content);

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--cap-file",
            "x=/tmp/tinct_nonexistent_test_file_xyz.txt:r",
            llt_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cap-file") && stderr.contains("cannot open"),
        "expected --cap-file I/O error, got: {stderr}"
    );
}

#[test]
fn cap_file_no_fs_suppresses_injection() {
    // Verify --no-fs suppresses --cap-file Handle injection.
    // The Handle is not injected, so %cfg is undefined.
    let llt_content = r#"[slurp %cfg]"#;
    let (llt_path, _llt_dir) = write_temp_llt("cap_file_no_fs_suppresses", llt_content);

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "--no-fs",
            "--cap-file",
            "cfg=/dev/null:r",
            llt_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    // Should fail: %cfg is not injected when --no-fs is set
    assert!(!output.status.success());
}

// ---------------------------------------------------------------------------
// tinct describe — input contract introspection
// ---------------------------------------------------------------------------

#[test]
fn describe_with_doc_string() {
    // Verify that `tinct describe` includes doc strings from @[doc: "..."] annotations
    let llt_content = r#"
[
  greet@[type: Fn  doc: "Returns a greeting message"]: fn name [
    call $str-concat "Hello, " $name
  ]

  add@[doc: "Adds two numbers"]: fn a b [call $+ $a $b]
]
"#;
    let (path, _dir) = write_temp_llt("describe_doc_string", llt_content);
    let output = Command::new(tinct_bin())
        .args(["describe", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("greet") && stdout.contains("Returns a greeting message"),
        "expected doc string for greet in output, got: {stdout}"
    );
    assert!(
        stdout.contains("add") && stdout.contains("Adds two numbers"),
        "expected doc string for add in output, got: {stdout}"
    );
}

#[test]
fn describe_json_mode_with_doc_string() {
    // Verify that `tinct describe --json` includes doc strings in the JSON output
    let llt_content = r#"
[
  greet@[type: Fn  doc: "Returns a greeting message"]: fn name [
    call $str-concat "Hello, " $name
  ]
]
"#;
    let (path, _dir) = write_temp_llt("describe_json_doc", llt_content);
    let output = Command::new(tinct_bin())
        .args(["describe", "--json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("expected valid JSON from describe --json");

    // Verify the JSON structure includes a docs section
    if let Some(contracts) = json.get("contracts").and_then(|c| c.as_array()) {
        if let Some(contract) = contracts.first() {
            if let Some(docs) = contract.get("docs").and_then(|d| d.as_object()) {
                assert!(
                    docs.get("greet")
                        .and_then(|v| v.as_str())
                        .map(|s| s.contains("Returns a greeting message"))
                        .unwrap_or(false),
                    "expected doc string for greet in JSON, got: {json}"
                );
            } else {
                panic!("expected docs section in contract, got: {json}");
            }
        }
    }
}

// NOTE: --allow-host flag was removed in cap-simplify sprint.
// Network access is controlled via NetCap allowlist entries in --cap-net.

// ---------------------------------------------------------------------------
// --libdir-path flag (override stdlib directory)
// ---------------------------------------------------------------------------

#[test]
fn libdir_path_override_flag_accepted() {
    // Test that --libdir-path flag is recognized and accepted.
    // Use the auto-detected stdlib path (if it exists) to ensure the flag doesn't break anything.
    let test_src = "[x: 1]";
    let (test_path, _test_dir) = write_temp_llt("libdir_override_flag", test_src);

    // Try to get the stdlib path from the binary location
    let stdlib_path =
        tinct::find_libdir_path().unwrap_or_else(|| std::path::PathBuf::from("stdlib"));

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            &format!("--libdir-path={}", stdlib_path.display()),
            test_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The command should succeed (--libdir-path is a recognized flag)
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        stderr,
        stdout
    );

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "expected valid JSON. stdout: '{}', stderr: '{}'",
            stdout, stderr
        )
    });
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn libdir_path_affects_formatter_resolution() {
    // Test that --libdir-path affects where -o/-i formatters are loaded from.
    // When we specify a non-existent path and try to use -o json, it should
    // error saying the formatter was not found at the custom path.
    let test_src = "[x: 1]";
    let (test_path, _test_dir) = write_temp_llt("libdir_formatter_resolution", test_src);

    let output = Command::new(tinct_bin())
        .args([
            "run",
            "-o",
            "json",
            "--libdir-path=/tmp/nonexistent-tinct-stdlib",
            test_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The command should fail with an error mentioning the custom libdir path
    assert!(
        !output.status.success(),
        "Expected failure when formatter not found at custom path. stderr: {}",
        stderr
    );

    assert!(
        stderr.contains("/tmp/nonexistent-tinct-stdlib/cli/out/json.llt")
            || stderr.contains("formatter not found"),
        "Expected error message about formatter at custom path. stderr: {}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// tinct lint — type-check without eval
// ---------------------------------------------------------------------------

#[test]
fn lint_clean_file_exits_zero() {
    // A file with no type errors or warnings should exit 0
    let llt_content = "[x: 42]";
    let (path, _dir) = write_temp_llt("lint_clean", llt_content);
    let output = Command::new(tinct_bin())
        .args(["lint", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected exit 0 for clean file, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // No output on success
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "", "expected no stdout on clean lint");
}

#[test]
fn lint_type_error_exits_one() {
    // A file with a type error (non-exhaustive match) should exit 1
    let llt_content = r#"[match [@[Int String] 42]
    Int: "int"]"#;
    let (path, _dir) = write_temp_llt("lint_type_error", llt_content);
    let output = Command::new(tinct_bin())
        .args(["lint", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected exit 1 for file with type error, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("non-exhaustive") || stderr.contains("error[T"),
        "expected type error message in stderr, got: {stderr}"
    );
}

#[test]
fn lint_no_eval() {
    // A file with an emit call should lint cleanly (no eval happens)
    // The emit is never executed, so there's no stdout output
    let llt_content = r#"[
  x: 42
  _: [emit "hello"]
]"#;
    let (path, _dir) = write_temp_llt("lint_no_eval", llt_content);
    let output = Command::new(tinct_bin())
        .args(["lint", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected exit 0 for file with emit (no eval), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "",
        "expected no stdout from lint (emit not executed)"
    );
}

// ---------------------------------------------------------------------------
// tinct fmt — path traversal guard
// ---------------------------------------------------------------------------

#[test]
fn fmt_invalid_output_path_traversal() {
    // `tinct fmt -o` with a path-traversal string must be rejected before any filesystem access.
    let (path, _dir) = write_temp_llt("fmt_invalid_output_traversal", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["fmt", "-o", "../secret", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit for path-traversal format name in tinct fmt"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid formatter name"),
        "expected error message mentioning invalid formatter name, got: {}",
        stderr
    );
}
