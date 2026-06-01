// Integration tests for CSV output formatter (stdlib/cli/out/csv.llt)
//
// These tests verify CSV formatter behavior that cannot be tested via corpus tests
// because the formatter writes to %stdout (not the return value) and depends on
// CLI-provided special variables (%emit-channel, %stdout) that don't exist in
// corpus test evaluation.
//
// Basic CSV functionality is covered in cli_tests.rs::output_flag_csv* tests.
// This file focuses on edge cases and CSV-specific behavior.

#![cfg(feature = "cli")]
#![allow(
    clippy::disallowed_methods,
    clippy::useless_format,
    clippy::to_string_in_format_args
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tinct_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tinct"))
}

/// A temporary directory that is automatically removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join("tinct_csv_tests").join(label);
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

fn write_temp_llt(name: &str, content: &str) -> (PathBuf, TempDir) {
    let dir = TempDir::new(name);
    let path = dir.path().join(format!("{}.llt", name));
    fs::write(&path, content).expect("failed to write temp file");
    (path, dir)
}

#[test]
fn csv_single_dict() {
    // A single dict (not in a list) produces header + one data row
    let source = r#"[name: "Charlie"  age: 35]"#;
    let (path, _dir) = write_temp_llt("csv_single_dict", source);
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
        stdout, "\"name\",\"age\"\n\"Charlie\",\"35\"\n",
        "single dict should produce header + one data row"
    );
}

#[test]
fn csv_missing_keys_produce_empty_cells() {
    // When subsequent records have different keys, missing keys produce empty cells.
    // First record: name, age → columns are ["name", "age"]
    // Second record: name only → age column gets empty string
    let source = r#"[0: [name: "Alice"  age: 30]  1: [name: "Bob"]]"#;
    let (path, _dir) = write_temp_llt("csv_missing_keys", source);
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
    // Second row has empty string for missing "age" field
    assert_eq!(
        stdout, "\"name\",\"age\"\n\"Alice\",\"30\"\n\"Bob\",\"\"\n",
        "missing keys should produce empty quoted cells"
    );
}

#[test]
fn csv_extra_keys_ignored() {
    // When subsequent records have extra keys not in the first record,
    // those keys are ignored (column order is fixed by first record).
    let source = r#"[0: [name: "Alice"  age: 30]  1: [name: "Bob"  age: 25  city: "NYC"]]"#;
    let (path, _dir) = write_temp_llt("csv_extra_keys", source);
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
    // "city" key is not in the header, so it's ignored
    assert_eq!(
        stdout, "\"name\",\"age\"\n\"Alice\",\"30\"\n\"Bob\",\"25\"\n",
        "extra keys in subsequent records should be ignored"
    );
}

#[test]
fn csv_quote_escaping() {
    // Values containing double quotes are escaped as "" (CSV standard)
    let source = r#"[0: [name: "Alice \"The Boss\""  title: "CEO"]]"#;
    let (path, _dir) = write_temp_llt("csv_quote_escape", source);
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
    // csv-quote replaces " with "" (CSV escape)
    // Input: Alice "The Boss" → Output: "Alice ""The Boss"""
    assert_eq!(
        stdout, "\"name\",\"title\"\n\"Alice \"\"The Boss\"\"\",\"CEO\"\n",
        "double quotes in values should be escaped as \"\""
    );
}

#[test]
fn csv_comma_in_value() {
    // Values containing commas are quoted (already happens — all fields are quoted)
    let source = r#"[0: [name: "Smith, John"  role: "Developer"]]"#;
    let (path, _dir) = write_temp_llt("csv_comma_in_value", source);
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
    // Commas inside quoted fields are safe
    assert_eq!(
        stdout, "\"name\",\"role\"\n\"Smith, John\",\"Developer\"\n",
        "commas in values should be preserved inside quotes"
    );
}

#[test]
fn csv_newline_in_value() {
    // Values containing newlines are quoted and preserved (CSV standard allows this)
    let source = "[0: [name: \"Alice\"  bio: \"line1\\nline2\"]]";
    let (path, _dir) = write_temp_llt("csv_newline_in_value", source);
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
    // Newlines inside quoted fields are preserved literally
    assert_eq!(
        stdout, "\"name\",\"bio\"\n\"Alice\",\"line1\\nline2\"\n",
        "newlines in values should be preserved inside quotes"
    );
}

#[test]
fn csv_empty_string_value() {
    // Empty string values produce empty quoted cells ""
    let source = r#"[0: [name: ""  age: 30]]"#;
    let (path, _dir) = write_temp_llt("csv_empty_string", source);
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
        stdout, "\"name\",\"age\"\n\"\",\"30\"\n",
        "empty string values should produce empty quoted cells"
    );
}

#[test]
fn csv_numeric_keys() {
    // Numeric dict keys (like "0", "1") are converted to strings for the header
    let source = r#"[0: [0: "Alice"  1: 30]  1: [0: "Bob"  1: 25]]"#;
    let (path, _dir) = write_temp_llt("csv_numeric_keys", source);
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
    // Keys are stringified: 0 → "0", 1 → "1"
    assert_eq!(
        stdout, "\"0\",\"1\"\n\"Alice\",\"30\"\n\"Bob\",\"25\"\n",
        "numeric keys should be stringified in header"
    );
}

#[test]
fn csv_boolean_values() {
    // Boolean values are converted to strings ("true", "false")
    let source = r#"[0: [active: true  verified: false]]"#;
    let (path, _dir) = write_temp_llt("csv_boolean", source);
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
    // Boolean values via str builtin: true → "true", false → "false"
    assert_eq!(
        stdout, "\"active\",\"verified\"\n\"true\",\"false\"\n",
        "boolean values should be stringified"
    );
}

#[test]
fn csv_float_values() {
    // Float values are converted to strings
    let source = r#"[0: [price: 19.99  tax: 1.5]]"#;
    let (path, _dir) = write_temp_llt("csv_float", source);
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
    // Float stringification depends on builtin str behavior
    assert!(
        stdout.contains("19.99") && stdout.contains("1.5"),
        "float values should be stringified, got: {stdout}"
    );
}

#[test]
fn csv_single_column() {
    // A dict with a single key produces a one-column CSV
    let source = r#"[0: [name: "Alice"]  1: [name: "Bob"]]"#;
    let (path, _dir) = write_temp_llt("csv_single_column", source);
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
        stdout, "\"name\"\n\"Alice\"\n\"Bob\"\n",
        "single-column CSV should have no trailing commas"
    );
}

#[test]
fn csv_many_columns() {
    // CSV with many columns (tests csv-header and csv-row iteration)
    let source = r#"[0: [a: 1  b: 2  c: 3  d: 4  e: 5  f: 6]]"#;
    let (path, _dir) = write_temp_llt("csv_many_columns", source);
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
        stdout, "\"a\",\"b\",\"c\",\"d\",\"e\",\"f\"\n\"1\",\"2\",\"3\",\"4\",\"5\",\"6\"\n",
        "CSV with many columns should preserve order"
    );
}

#[test]
fn csv_many_rows() {
    // CSV with many rows (tests loop iteration)
    let source = r#"[
        0: [x: 1]
        1: [x: 2]
        2: [x: 3]
        3: [x: 4]
        4: [x: 5]
        5: [x: 6]
        6: [x: 7]
        7: [x: 8]
        8: [x: 9]
        9: [x: 10]
    ]"#;
    let (path, _dir) = write_temp_llt("csv_many_rows", source);
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
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 11, "expected header + 10 data rows");
    assert_eq!(lines[0], "\"x\"", "header should be 'x'");
    assert_eq!(lines[10], "\"10\"", "last row should be '10'");
}

#[test]
fn csv_column_order_from_first_record() {
    // Column order is determined by the first record's key order
    let source = r#"[0: [z: 1  a: 2  m: 3]  1: [a: 4  m: 5  z: 6]]"#;
    let (path, _dir) = write_temp_llt("csv_column_order", source);
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
    // First record has keys in order: z, a, m
    // Second record has different order but should follow first record's order
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0], "\"z\",\"a\",\"m\"",
        "header order from first record"
    );
    assert_eq!(lines[1], "\"1\",\"2\",\"3\"", "first data row");
    assert_eq!(
        lines[2], "\"6\",\"4\",\"5\"",
        "second data row uses first record's column order (z=6, a=4, m=5)"
    );
}

#[test]
fn csv_unicode_values() {
    // Unicode characters in values are preserved
    let source = r#"[0: [name: "José"  city: "São Paulo"  emoji: "🎉"]]"#;
    let (path, _dir) = write_temp_llt("csv_unicode", source);
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
    assert!(
        stdout.contains("José") && stdout.contains("São Paulo") && stdout.contains("🎉"),
        "unicode values should be preserved, got: {stdout}"
    );
}

#[test]
fn csv_all_missing_keys() {
    // Edge case: all records after the first have completely different keys
    let source = r#"[0: [a: 1]  1: [b: 2]  2: [c: 3]]"#;
    let (path, _dir) = write_temp_llt("csv_all_missing", source);
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
    // Header: "a" (from first record)
    // Row 1: "1"
    // Row 2: "" (b is not in header)
    // Row 3: "" (c is not in header)
    assert_eq!(
        stdout, "\"a\"\n\"1\"\n\"\"\n\"\"\n",
        "records with completely different keys produce empty cells"
    );
}

#[test]
fn csv_special_characters_in_keys() {
    // Column names (keys) can contain special characters
    let source = r#"[0: ["first-name": "Alice"  "last.name": "Smith"]]"#;
    let (path, _dir) = write_temp_llt("csv_special_keys", source);
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
        stdout, "\"first-name\",\"last.name\"\n\"Alice\",\"Smith\"\n",
        "special characters in keys should be quoted in header"
    );
}
