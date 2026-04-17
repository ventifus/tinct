//! Integration tests for the `llt` CLI binary.
//!
//! These tests exercise the CLI (main.rs) via `std::process::Command`,
//! covering subcommands, output formats, flags, and error cases.
//! The binary requires the `cli` feature, so we gate the entire file.

#![cfg(feature = "cli")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Return the path to the compiled `llt` binary.
/// `CARGO_BIN_EXE_llt` is set by Cargo during test compilation when a
/// `[[bin]]` named `llt` exists.
fn llt_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_llt"))
}

/// Create a temporary LLT file with the given content and return its path.
/// Uses the test name (via a caller-supplied label) to make filenames unique
/// so parallel tests never collide.
fn write_temp_llt(label: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("llt_cli_tests");
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    let path = dir.join(format!("{label}.llt"));
    fs::write(&path, content).expect("failed to write temp file");
    path
}

// ---------------------------------------------------------------------------
// Basic evaluation — default JSON output
// ---------------------------------------------------------------------------

#[test]
fn eval_simple_dict_json_output() {
    let path = write_temp_llt("eval_simple_dict", "[x: 1 y: hello]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON output");
    assert_eq!(json, serde_json::json!({"x": 1, "y": "hello"}));
}

#[test]
fn eval_scalar_int() {
    let path = write_temp_llt("eval_scalar_int", "42");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

#[test]
fn eval_scalar_string() {
    let path = write_temp_llt("eval_scalar_string", "\"hello world\"");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!("hello world"));
}

#[test]
fn eval_scalar_bool() {
    let path = write_temp_llt("eval_scalar_bool", "true");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(true));
}

#[test]
fn eval_scalar_float() {
    let path = write_temp_llt("eval_scalar_float", "3.14");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(3.14));
}

#[test]
fn eval_array_like_dict() {
    let path = write_temp_llt("eval_array_like", "[10 20 30]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!([10, 20, 30]));
}

#[test]
fn eval_nested_dict() {
    let path = write_temp_llt("eval_nested", "[a: [b: [c: 42]]]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    let path = write_temp_llt("eval_format_json", "[x: 1]");
    let output = Command::new(llt_bin())
        .args(["eval", "--format", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"x": 1}));
}

#[test]
fn eval_format_json_short_flag() {
    let path = write_temp_llt("eval_format_json_short", "[x: 1]");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "json", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    let path = write_temp_llt("eval_format_llt_scalar", "42");
    let output = Command::new(llt_bin())
        .args(["eval", "--format", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Int(42)");
}

#[test]
fn eval_format_llt_dict() {
    let path = write_temp_llt("eval_format_llt_dict", "[x: 42]");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Dict({\"x\": Int(42)})");
}

#[test]
fn eval_format_llt_string() {
    let path = write_temp_llt("eval_format_llt_string", "\"hello\"");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "String(\"hello\")");
}

#[test]
fn eval_format_llt_bool() {
    let path = write_temp_llt("eval_format_llt_bool", "true");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Bool(true)");
}

#[test]
fn eval_format_llt_float() {
    let path = write_temp_llt("eval_format_llt_float", "3.14");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    let path = write_temp_llt("eval_flag_deep", "[a: [b: [c: 42]]]");
    let output = Command::new(llt_bin())
        .args(["eval", "--eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"a": {"b": {"c": 42}}}));
}

#[test]
fn eval_flag_with_llt_format() {
    let path = write_temp_llt("eval_flag_llt", "[x: 1]");
    let output = Command::new(llt_bin())
        .args(["eval", "--eval", "-f", "llt", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "Dict({\"x\": Int(1)})");
}

// ---------------------------------------------------------------------------
// Multi-document pipeline
// ---------------------------------------------------------------------------

#[test]
fn eval_multi_document_pipeline() {
    // doc1 produces {x: 10}, doc2 receives it as $$ and wraps it
    let source = "[x: 10]\n---\n[result: $$.x]";
    let path = write_temp_llt("eval_multi_doc", source);
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": 10}));
}

// ---------------------------------------------------------------------------
// Expressions with stdlib builtins
// ---------------------------------------------------------------------------

#[test]
fn eval_builtin_add() {
    let path = write_temp_llt("eval_builtin_add", "[call $+ 1 2]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(3));
}

#[test]
fn eval_builtin_if() {
    let path = write_temp_llt("eval_builtin_if", "[call $if true 42 99]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!(42));
}

// ---------------------------------------------------------------------------
// Error cases
// ---------------------------------------------------------------------------

#[test]
fn eval_missing_file() {
    let output = Command::new(llt_bin())
        .args(["eval", "/tmp/llt_cli_tests/nonexistent_file.llt"])
        .output()
        .expect("failed to run llt");

    assert!(!output.status.success(), "expected non-zero exit for missing file");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error reading file"),
        "expected 'error reading file' in stderr, got: {stderr}"
    );
}

#[test]
fn eval_missing_file_exit_code() {
    let output = Command::new(llt_bin())
        .args(["eval", "/tmp/llt_cli_tests/no_such_file.llt"])
        .output()
        .expect("failed to run llt");

    // main.rs exits with code 1 for errors
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn eval_parse_error() {
    // Unterminated bracket is a parse error
    let path = write_temp_llt("eval_parse_error", "[x: 1");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(!output.status.success(), "expected non-zero exit for parse error");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The error message should be non-empty
    assert!(!stderr.trim().is_empty(), "expected error message on stderr");
}

#[test]
fn eval_error_undefined_var() {
    // Referencing an undefined variable should produce an eval error
    let path = write_temp_llt("eval_error_undef", "$nonexistent");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.trim().is_empty(), "expected error message for undefined var");
}

// ---------------------------------------------------------------------------
// No subcommand / help
// ---------------------------------------------------------------------------

#[test]
fn no_subcommand_shows_usage() {
    let output = Command::new(llt_bin())
        .output()
        .expect("failed to run llt");

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
    let output = Command::new(llt_bin())
        .args(["--help"])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Lazy Lisp Transformer"),
        "expected description in help output, got: {stdout}"
    );
}

#[test]
fn eval_help_flag() {
    let output = Command::new(llt_bin())
        .args(["eval", "--help"])
        .output()
        .expect("failed to run llt");

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
    let path = write_temp_llt("eval_invalid_format", "42");
    let output = Command::new(llt_bin())
        .args(["eval", "-f", "xml", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    let output = Command::new(llt_bin())
        .args(["--version"])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("llt"),
        "expected 'llt' in version output, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// JSON output is well-formed pretty-printed
// ---------------------------------------------------------------------------

#[test]
fn eval_json_output_is_pretty_printed() {
    let path = write_temp_llt("eval_pretty_json", "[a: 1 b: 2]");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    // When stdin is piped with JSON, it should be available as $$ in the first doc
    let path = write_temp_llt("eval_stdin_json", "[name: $$.name]");
    let output = Command::new(llt_bin())
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

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
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
    let path = write_temp_llt("eval_empty", "  \n  ");
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

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
    let path = write_temp_llt("eval_scope_chain", source);
    let output = Command::new(llt_bin())
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("failed to run llt");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json, serde_json::json!({"result": 10}));
}
