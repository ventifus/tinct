//! Integration tests for the `tinct` CLI binary.
//!
//! These tests exercise the CLI (main.rs) via `std::process::Command`,
//! covering subcommands, output formats, flags, and error cases.
//! The binary requires the `cli` feature, so we gate the entire file.

#![cfg(feature = "cli")]

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
    let (path, _dir) = write_temp_llt("eval_simple_dict", "[x: 1 y: hello]");
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"a": {"b": {"c": 42}}}));
}

// ---------------------------------------------------------------------------
// --format json (explicit)
// ---------------------------------------------------------------------------

#[test]
fn eval_format_json_explicit() {
    let (path, _dir) = write_temp_llt("eval_format_json", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["eval", "--format", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn eval_format_json_short_flag() {
    let (path, _dir) = write_temp_llt("eval_format_json_short", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["eval", "-f", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1}));
}

// ---------------------------------------------------------------------------
// --format llt
// ---------------------------------------------------------------------------

#[test]
fn eval_format_llt_scalar() {
    let (path, _dir) = write_temp_llt("eval_format_llt_scalar", "42");
    let output = Command::new(tinct_bin())
        .args(["eval", "--format", "llt", path.to_str().unwrap()])
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
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
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
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
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
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
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
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Float(3.14)");
}

// ---------------------------------------------------------------------------
// --eval flag (deep materialization)
// ---------------------------------------------------------------------------

#[test]
fn eval_flag_deep_materialize() {
    // Without --eval, lazy thunks may not be forced. With --eval, all
    // thunks are deep-materialized before output. Both should produce
    // the same JSON for this simple case, but --eval exercises the
    // deep_materialize code path in main.rs.
    let (path, _dir) = write_temp_llt("eval_flag_deep", "[a: [b: [c: 42]]]");
    let output = Command::new(tinct_bin())
        .args(["eval", "--eval", path.to_str().unwrap()])
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
        .args(["eval", "--eval", "-f", "llt", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", "/tmp/llt_cli_tests/nonexistent_file.llt"])
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
        .args(["eval", "/tmp/llt_cli_tests/no_such_file.llt"])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", "--help"])
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
        .args(["eval", "-f", "xml", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // clap should reject the invalid format value
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value") || stderr.contains("possible values"),
        "expected clap validation error, got: {stderr}"
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
fn eval_json_output_is_pretty_printed() {
    let (path, _dir) = write_temp_llt("eval_pretty_json", "[a: 1 b: 2]");
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Pretty-printed JSON has newlines and indentation
    assert!(
        stdout.contains('\n'),
        "expected pretty-printed JSON with newlines, got: {stdout}"
    );
    assert!(
        stdout.contains("  "),
        "expected indentation in pretty-printed JSON, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Stdin JSON injection (piped via stdin to the child process)
// ---------------------------------------------------------------------------

#[test]
fn eval_stdin_json_injection() {
    // When stdin is piped with JSON, it should be available as % in the first doc
    let (path, _dir) = write_temp_llt("eval_stdin_json", "[name: %.name]");
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
fn include_basic_dict() {
    let dir = make_include_dir("basic_dict");
    let helper = dir.path().join("helper.llt");
    fs::write(&helper, "[x: 1 y: 2]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[result: [call $include \"helper.llt\"]]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", main.to_str().unwrap()])
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
fn include_namespaced() {
    // Include a helper and access its fields via the namespace binding
    let dir = make_include_dir("namespaced");
    fs::write(
        dir.path().join("helper.llt"),
        "[double: [fn [n] [call $* $n 2]]]",
    )
    .unwrap();
    let main_src = r#"[utils: [call $include "helper.llt"]]
[result: [call $utils.double 21]]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_merged_scope_chain() {
    // First expression is an include (result merges into scope), second uses its bindings
    let dir = make_include_dir("merged_scope");
    fs::write(dir.path().join("helper.llt"), "[x: 10 y: 20]").unwrap();
    let main_src = "[call $include \"helper.llt\"]\n[sum: [call $+ $x $y]]";
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_nested_a_includes_b_includes_c() {
    // A includes B, B includes C — nested transitive include
    let dir = make_include_dir("nested_chain");
    fs::write(dir.path().join("c.llt"), "[val: 99]").unwrap();
    fs::write(
        dir.path().join("b.llt"),
        "[inner: [call $include \"c.llt\"]]",
    )
    .unwrap();
    fs::write(
        dir.path().join("a.llt"),
        "[outer: [call $include \"b.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("a.llt").to_str().unwrap()])
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
fn include_circular_error() {
    // A includes B, B includes A — circular dependency
    let dir = make_include_dir("circular");
    fs::write(dir.path().join("a.llt"), "[call $include \"b.llt\"]").unwrap();
    fs::write(dir.path().join("b.llt"), "[call $include \"a.llt\"]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("a.llt").to_str().unwrap()])
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
fn include_self_circular_error() {
    // File includes itself — degenerate circular case
    let dir = make_include_dir("self_circular");
    fs::write(dir.path().join("self.llt"), "[call $include \"self.llt\"]").unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("self.llt").to_str().unwrap()])
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
fn include_file_not_found_error() {
    let dir = make_include_dir("file_not_found");
    fs::write(
        dir.path().join("main.llt"),
        "[call $include \"nonexistent.llt\"]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_relative_path_from_subdirectory() {
    // Main file in root dir includes a file in a subdirectory via relative path
    let dir = make_include_dir("relative_subdir");
    let sub = dir.path().join("lib");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("utils.llt"), "[pi: 3.14]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[math: [call $include \"lib/utils.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
        "[call $include \"b.llt\"]\n[nested: $val]",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[wrapper: [call $include \"sub/a.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_with_stdlib_builtins() {
    // Included file uses stdlib builtins (arithmetic)
    let dir = make_include_dir("stdlib_builtins");
    fs::write(dir.path().join("math.llt"), "[sum: [call $+ 10 20]]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[result: [call $include \"math.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_returns_scalar() {
    // Included file evaluates to a scalar (not a dict)
    let dir = make_include_dir("scalar_return");
    fs::write(dir.path().join("answer.llt"), "42").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[answer: [call $include \"answer.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_returns_string() {
    // Included file evaluates to a string scalar
    let dir = make_include_dir("string_return");
    fs::write(dir.path().join("greeting.llt"), "\"hello world\"").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[msg: [call $include \"greeting.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_diamond_pattern_no_cycle() {
    // Diamond: main includes A and B, both include C — NOT circular
    // (C is included twice but never re-enters while already in the guard)
    let dir = make_include_dir("diamond");
    fs::write(dir.path().join("c.llt"), "[shared: 100]").unwrap();
    fs::write(
        dir.path().join("a.llt"),
        "[a_data: [call $include \"c.llt\"]]",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.llt"),
        "[b_data: [call $include \"c.llt\"]]",
    )
    .unwrap();
    let main_src = r#"[
  a: [call $include "a.llt"]
  b: [call $include "b.llt"]
]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_isolation_no_caller_scope() {
    // Included file should NOT see bindings from the caller's scope
    let dir = make_include_dir("isolation");
    // The included file tries to reference $caller_var which is only in main's scope
    fs::write(dir.path().join("helper.llt"), "[val: $caller_var]").unwrap();
    let main_src = "[caller_var: 999]\n[result: [call $include \"helper.llt\"]]";
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_with_deep_materialize() {
    // Use --eval flag with includes to exercise deep materialization
    let dir = make_include_dir("deep_materialize");
    fs::write(dir.path().join("nested.llt"), "[a: [b: [c: 42]]]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[data: [call $include \"nested.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "eval",
            "--eval",
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
fn include_llt_format_output() {
    // Include test with LLT display format output
    let dir = make_include_dir("llt_format");
    fs::write(dir.path().join("helper.llt"), "[x: 42]").unwrap();
    fs::write(
        dir.path().join("main.llt"),
        "[call $include \"helper.llt\"]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args([
            "eval",
            "-f",
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
        "[data: [call $include \"../parent.llt\"]]",
    )
    .unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", subdir.join("child.llt").to_str().unwrap()])
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
    let main_src = r#"[get_name: [call $include "mapper.llt"]]
[result: [call $get_name [name: "Alice"]]]"#;
    fs::write(dir.path().join("main.llt"), main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_with_dircap() {
    // Test the new cap-qualified include pattern: [include $cap "path"]
    let dir = make_include_dir("dircap_include");

    // Create a helper file in the test directory
    fs::write(dir.path().join("data.llt"), "[value: 42]").unwrap();

    // Main file uses dir-cap to create a capability, then includes via that cap.
    // Use a scope chain to avoid serializing the DirCap itself.
    let main_src = format!(
        r#"[dir-cap "{}"]
---
[include % "data.llt"]"#,
        dir.path().display()
    );
    fs::write(dir.path().join("main.llt"), &main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
fn include_with_dircap_and_hash() {
    // Test cap-qualified include with integrity hash: [include $cap "path" "hash"]
    let dir = make_include_dir("dircap_hash");

    // Create a helper file
    let content = "[value: 99]";
    fs::write(dir.path().join("data.llt"), content).unwrap();

    // Compute the blake3 hash of the content
    let hash = blake3::hash(content.as_bytes());
    let hash_hex = hash.to_hex();

    // Main file uses dir-cap and includes with hash verification.
    // Use a scope chain to avoid serializing the DirCap itself.
    let main_src = format!(
        r#"[dir-cap "{}"]
---
[include % "data.llt" "blake3:{}"]"#,
        dir.path().display(),
        hash_hex
    );
    fs::write(dir.path().join("main.llt"), &main_src).unwrap();

    let output = Command::new(tinct_bin())
        .args(["eval", dir.path().join("main.llt").to_str().unwrap()])
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
    let (path, _dir) = write_temp_llt("no_fs_flag", "[call $include \"some_file.llt\"]");
    let output = Command::new(tinct_bin())
        .args(["eval", "--no-fs", path.to_str().unwrap()])
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
        stderr.contains("filesystem access is disabled") || stderr.contains("E042"),
        "expected error message about disabled filesystem access, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// --allow-path flag (filesystem allowlist)
// ---------------------------------------------------------------------------

#[test]
fn allow_path_permits_include_from_allowed_dir() {
    // With --allow-path <dir>, $include from that directory succeeds.
    let dir = TempDir::new("allow_path_permit");
    let included_path = dir.path().join("lib.llt");
    fs::write(&included_path, "[value: 42]").expect("failed to write lib.llt");
    let main_path = dir.path().join("allow_path_permit.llt");
    fs::write(&main_path, "[call $include \"lib.llt\"]").expect("failed to write main llt");

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args(["eval", "--allow-path", dir_str, main_path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "expected success with --allow-path pointing to the include directory; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"value": 42}));
}

#[test]
fn allow_path_blocks_include_outside_allowed_dir() {
    // With --allow-path pointing to a different directory, $include from the
    // main file's directory is blocked (E057).
    //
    // Two separate temp dirs are used:
    //   - `allowed_dir`: the directory passed to --allow-path (not where the
    //     include target lives)
    //   - `main_dir`: where both the main LLT file and the included file live
    //     (outside the allowed dir)
    //
    // Since `allowed_dir` is not a prefix of `main_dir`, $include is blocked.
    let allowed_dir = TempDir::new("allow_path_allowed");
    let main_dir = TempDir::new("allow_path_block");

    // Write the included file into main_dir (outside allowed_dir).
    let included_path = main_dir.path().join("secret.llt");
    fs::write(&included_path, "[secret: true]").expect("failed to write secret.llt");

    // Write the main file into main_dir.
    let main_path = main_dir.path().join("allow_path_block.llt");
    fs::write(&main_path, "[call $include \"secret.llt\"]").expect("failed to write main llt");

    // Canonicalize allowed_dir so the comparison is stable even under symlinks.
    let allowed_canonical =
        fs::canonicalize(allowed_dir.path()).expect("failed to canonicalize allowed_dir");

    let output = Command::new(tinct_bin())
        .args([
            "eval",
            "--allow-path",
            allowed_canonical.to_str().unwrap(),
            main_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        !output.status.success(),
        "expected non-zero exit when $include is outside --allow-path"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 (error) for allowlist violation"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not permitted by the --allow-path allowlist") || stderr.contains("E057"),
        "expected allowlist error message, got: {stderr}"
    );
}

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
        .args(["eval", "--timeout", "1s", path.to_str().unwrap()])
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
        .args(["eval", "--no-fs", path.to_str().unwrap()])
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
        .args(["eval", "--timeout", "5s", path.to_str().unwrap()])
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
        .args(["eval", "--timeout", "abc", path.to_str().unwrap()])
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
        .args(["eval", "--no-fs", "--timeout", "5s", path.to_str().unwrap()])
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
        "[call $include \"some_file.llt\"]",
    );
    let output = Command::new(tinct_bin())
        .args(["eval", "--no-fs", "--timeout", "5s", path.to_str().unwrap()])
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
        stderr.contains("filesystem access is disabled") || stderr.contains("E042"),
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
        .args(["eval", path.to_str().unwrap()])
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
fn eval_deep_materialize_seq() {
    // Test deep materialization with a sequence
    let (path, _dir) = write_temp_llt(
        "deep_materialize_seq",
        "[call $collect [call $take 3 [call $range 0 10]]]",
    );
    let output = Command::new(tinct_bin())
        .args(["eval", "--eval", path.to_str().unwrap()])
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
    // Even without --allow-path, passing --no-landlock should not error.
    let (path, _dir) = write_temp_llt("no_landlock_flag", "[x: 1]");
    let output = Command::new(tinct_bin())
        .args(["eval", "--no-landlock", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    // Must succeed — the flag is a no-op when no --allow-path is given.
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
fn no_landlock_with_allow_path_accepted() {
    // --no-landlock combined with --allow-path must be accepted. The flag
    // disables Landlock kernel enforcement while still using the application-
    // level allowlist. This is the graceful degradation path for kernels < 5.13
    // or environments where Landlock is unavailable.
    let dir = TempDir::new("no_landlock_allow_path");
    let included = dir.path().join("data.llt");
    fs::write(&included, "[value: 99]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[call $include \"data.llt\"]").unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args([
            "eval",
            "--no-landlock",
            "--allow-path",
            dir_str,
            main.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "--no-landlock with --allow-path should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("expected valid JSON");
    assert_eq!(json, serde_json::json!({"value": 99}));
}

#[test]
#[cfg(target_os = "linux")]
fn landlock_with_allow_path_permits_include() {
    // On Linux, --allow-path activates Landlock by default. $include from the
    // allowed directory must succeed. This test confirms Landlock does not
    // accidentally block access to explicitly allowed paths.
    let dir = TempDir::new("landlock_allow_path_permit");
    let included = dir.path().join("lib.llt");
    fs::write(&included, "[result: 42]").unwrap();
    let main = dir.path().join("main.llt");
    fs::write(&main, "[call $include \"lib.llt\"]").unwrap();

    let dir_str = dir.path().to_str().unwrap();
    let output = Command::new(tinct_bin())
        .args(["eval", "--allow-path", dir_str, main.to_str().unwrap()])
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
            "eval",
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
        .args(["eval", "--max-memory", "0", path.to_str().unwrap()])
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
        .args(["eval", "--max-cpu", "10", path.to_str().unwrap()])
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
        .args(["eval", "--max-fds", "32", path.to_str().unwrap()])
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
        .args(["eval", "--max-fds", "0", path.to_str().unwrap()])
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
            "eval",
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
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
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // emit suppresses JSON output, so stdout should just be "hello world\n"
    assert_eq!(stdout, "hello world\n");
}

#[test]
fn env_missing() {
    let (path, _dir) = write_temp_llt("env_missing", "[env \"NONEXISTENT_VAR_12345\"]");
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // env returns null (empty dict) for missing vars
    assert_eq!(json, serde_json::json!({}));
}

#[test]
fn env_no_env_flag() {
    let (path, _dir) = write_temp_llt("env_no_env", "[env \"PATH\"]");
    let output = Command::new(tinct_bin())
        .args(["eval", "--no-env", path.to_str().unwrap()])
        .output()
        .expect("failed to run tinct");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    // --no-env makes env return null for all vars
    assert_eq!(json, serde_json::json!({}));
}

#[test]
fn revocable_and_revoke() {
    // Test revocable DirCap creation and revocation
    let dir = TempDir::new("revocable_test");
    let test_file = dir.path().join("data.txt");
    fs::write(&test_file, "test content").expect("failed to write test file");

    let llt_content = format!(
        r#"
[cap: [dir-cap "{}"]]
[revocable-cap: [revocable cap]]
[fh: [open revocable-cap "data.txt" "r"]]
[content: [slurp fh]]
[_ : [revoke-cap revocable-cap]]
content
"#,
        dir.path().display()
    );
    let (path, _llt_dir) = write_temp_llt("revocable_revoke", &llt_content);
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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

    let llt_content = format!(
        r#"
[cap: [dir-cap "{}"]]
[fh: [open cap "lines.txt" "r"]]
[collect [take 2 [lines fh]]]
"#,
        dir.path().display()
    );
    let (path, _llt_dir) = write_temp_llt("lines_basic", &llt_content);
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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

    let llt_content = format!(
        r#"
[cap: [dir-cap "{}"]]
[_ : [write cap "output.txt" "hello world"]]
[fh: [open cap "output.txt" "r"]]
[slurp fh]
"#,
        dir.path().display()
    );
    let (path, _llt_dir) = write_temp_llt("write_basic", &llt_content);
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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

    let llt_content = format!(
        r#"
[cap: [dir-cap "{}"]]
[_ : [write-atomic cap "output.txt" "atomic content"]]
[fh: [open cap "output.txt" "r"]]
[slurp fh]
"#,
        dir.path().display()
    );
    let (path, _llt_dir) = write_temp_llt("write_atomic_basic", &llt_content);
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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
fn write_and_slurp_roundtrip() {
    // Test write + slurp roundtrip via stdlib/io.llt wrappers
    let dir = TempDir::new("write_roundtrip_test");

    let llt_content = format!(
        r#"
[include libdir "io.llt"]
[cap: [dir-cap "{}"]]
[_ : [write-file cap "test.txt" "roundtrip data"]]
[read-file cap "test.txt"]
"#,
        dir.path().display()
    );
    let (path, _llt_dir) = write_temp_llt("write_roundtrip", &llt_content);
    let output = Command::new(tinct_bin())
        .args(["eval", path.to_str().unwrap()])
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
            "eval",
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
    // Test multi-file pipeline with emit (text output instead of JSON)
    let dir = TempDir::new("multi_file_emit");

    let data_path = dir.path().join("data.llt");
    fs::write(&data_path, "[name: \"Alice\"  greeting: \"Hello\"]")
        .expect("failed to write data file");

    let format_path = dir.path().join("format.llt");
    fs::write(&format_path, "[emit [str %.greeting \", \" %.name \"!\"]]")
        .expect("failed to write format file");

    let output = Command::new(tinct_bin())
        .args([
            "eval",
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
    // emit writes to stdout directly without JSON serialization
    assert_eq!(stdout, "Hello, Alice!");
}
