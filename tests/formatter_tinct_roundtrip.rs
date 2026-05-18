// Integration test for tinct-hosted formatters (compact and pretty)
// Verifies that formatting is idempotent and parseable

use std::path::PathBuf;
use tinct::{format_source_tinct, parse};

fn compact_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib")
        .join("cli")
        .join("fmt")
        .join("compact.llt")
}

fn pretty_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib")
        .join("cli")
        .join("fmt")
        .join("pretty.llt")
}

#[test]
fn test_tinct_formatter_compact_simple_dict() {
    let input = r#"[
  server: [
    port: 8080
    host: "localhost"
  ]
  enabled: true
]"#;

    // Format with tinct compact formatter
    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");

    // Should produce output
    assert!(!formatted.is_empty(), "formatter produced empty output");

    // Output should be parseable
    parse(&formatted).expect("formatted output is not parseable");

    // Formatting should be idempotent (formatting twice gives same result)
    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again, "formatter is not idempotent");
}

#[test]
fn test_tinct_formatter_pretty_simple_dict() {
    let input = r#"[
  server: [
    port: 8080
    host: "localhost"
  ]
  enabled: true
]"#;

    // Format with tinct pretty formatter
    let formatted = format_source_tinct(input, &pretty_script()).expect("formatter failed");

    // Should produce output
    assert!(!formatted.is_empty(), "formatter produced empty output");

    // Output should be parseable
    parse(&formatted).expect("formatted output is not parseable");

    // Formatting should be idempotent
    let formatted_again =
        format_source_tinct(&formatted, &pretty_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again, "formatter is not idempotent");
}

#[test]
fn test_tinct_formatter_compact_literals() {
    let input = r#"[
  int: 42
  float: 3.14
  bool: true
  str: "hello"
]"#;

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");

    // Should parse
    let parsed = parse(&formatted).expect("formatted output is not parseable");

    // Should have one document with one dict expression
    assert_eq!(parsed.node.documents.len(), 1);
    assert_eq!(parsed.node.documents[0].node.expressions.len(), 1);
}

#[test]
fn test_tinct_formatter_pretty_nested_dict() {
    let input = r#"[
  outer: [
    inner: [
      deep: 123
    ]
  ]
]"#;

    let formatted = format_source_tinct(input, &pretty_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again =
        format_source_tinct(&formatted, &pretty_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_function() {
    let input = "[add: [fn [x y] [+ x y]]]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_call() {
    let input = "[[fn [x] [+ x 1]] 42]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_empty_dict() {
    let input = "[]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    assert_eq!(formatted, "[]\n", "empty dict not formatted correctly");

    parse(&formatted).expect("formatted output is not parseable");
}

#[test]
fn test_tinct_formatter_compact_auto_indexed() {
    let input = "[1 2 3]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_keyed_entry() {
    // Tests that a keyed dict entry formats correctly (key: value syntax)
    let input = "[port: 8080]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    assert!(
        formatted.contains("port"),
        "formatted output missing key 'port': {formatted}"
    );
    assert!(
        formatted.contains("8080"),
        "formatted output missing value 8080: {formatted}"
    );
    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_multiline_to_oneline() {
    // The compact formatter collapses multi-line source to one line
    let input = "[\n  port: 8080\n  host: \"localhost\"\n]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");

    // Compact output should not have interior newlines (only trailing newline)
    let trimmed = formatted.trim();
    assert!(
        !trimmed.contains('\n'),
        "compact formatter should not produce newlines in dict body: {formatted}"
    );

    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_pretty_comments_preserved() {
    // The pretty formatter preserves leading comments in block-mode dicts.
    // A dict is rendered in block mode when it has >4 entries OR is too wide.
    // Use a dict large enough to force block mode.
    let input = concat!(
        "[\n",
        "  # server configuration\n",
        "  port: 8080\n",
        "  host: \"localhost\"\n",
        "  workers: 4\n",
        "  timeout: 30\n",
        "  max-connections: 100\n",
        "]"
    );

    let formatted = format_source_tinct(input, &pretty_script()).expect("formatter failed");

    // 5 entries → block mode → comments are preserved
    assert!(
        formatted.contains("# server configuration"),
        "pretty formatter should preserve comments in block-mode dicts: {formatted}"
    );

    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &pretty_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_string_quoted() {
    // In compact mode (no source info), string literals are always quoted
    let input = r#"[host: "localhost"]"#;

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    assert!(
        formatted.contains('"'),
        "compact formatter should quote string literals: {formatted}"
    );
    parse(&formatted).expect("formatted output is not parseable");
}

#[test]
fn test_tinct_formatter_compact_bool() {
    let input = "[enabled: true disabled: false]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    assert!(formatted.contains("true"), "missing 'true': {formatted}");
    assert!(formatted.contains("false"), "missing 'false': {formatted}");
    parse(&formatted).expect("formatted output is not parseable");
}

#[test]
fn test_tinct_formatter_compact_match_expr() {
    let input = "[result: [match x 1: \"one\" 2: \"two\" _: \"other\"]]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_pretty_multi_document() {
    let input = "[x: 1]\n---\n[y: 2]";

    let formatted = format_source_tinct(input, &pretty_script()).expect("formatter failed");
    assert!(
        formatted.contains("---"),
        "pretty formatter should preserve document separators: {formatted}"
    );
    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &pretty_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_compact_multi_document() {
    let input = "[x: 1]\n---\n[y: 2]";

    let formatted = format_source_tinct(input, &compact_script()).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    let formatted_again =
        format_source_tinct(&formatted, &compact_script()).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}
