// Integration test for tinct-hosted compact formatter
// Verifies that formatting is idempotent and parseable

use tinct::{format_source_tinct, parse};

#[test]
fn test_tinct_formatter_simple_dict() {
    let input = r#"[
  server: [
    port: 8080
    host: "localhost"
  ]
  enabled: true
]"#;

    // Format with tinct formatter
    let formatted = format_source_tinct(input).expect("formatter failed");

    // Should produce output
    assert!(!formatted.is_empty(), "formatter produced empty output");

    // Output should be parseable
    parse(&formatted).expect("formatted output is not parseable");

    // Formatting should be idempotent (formatting twice gives same result)
    let formatted_again = format_source_tinct(&formatted).expect("second format failed");
    assert_eq!(formatted, formatted_again, "formatter is not idempotent");
}

#[test]
fn test_tinct_formatter_literals() {
    let input = r#"[
  int: 42
  float: 3.14
  bool: true
  str: "hello"
]"#;

    let formatted = format_source_tinct(input).expect("formatter failed");

    // Should parse
    let parsed = parse(&formatted).expect("formatted output is not parseable");

    // Should have one document with one dict expression
    assert_eq!(parsed.node.documents.len(), 1);
    assert_eq!(parsed.node.documents[0].node.expressions.len(), 1);
}

#[test]
fn test_tinct_formatter_nested_dict() {
    let input = r#"[
  outer: [
    inner: [
      deep: 123
    ]
  ]
]"#;

    let formatted = format_source_tinct(input).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again = format_source_tinct(&formatted).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_function() {
    let input = "[add: [fn [x y] [+ x y]]]";

    let formatted = format_source_tinct(input).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again = format_source_tinct(&formatted).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_call() {
    let input = "[[fn [x] [+ x 1]] 42]";

    let formatted = format_source_tinct(input).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again = format_source_tinct(&formatted).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}

#[test]
fn test_tinct_formatter_empty_dict() {
    let input = "[]";

    let formatted = format_source_tinct(input).expect("formatter failed");
    assert_eq!(
        formatted.trim(),
        "[]\n",
        "empty dict not formatted correctly"
    );

    parse(&formatted).expect("formatted output is not parseable");
}

#[test]
fn test_tinct_formatter_auto_indexed() {
    let input = "[1 2 3]";

    let formatted = format_source_tinct(input).expect("formatter failed");
    parse(&formatted).expect("formatted output is not parseable");

    // Idempotent
    let formatted_again = format_source_tinct(&formatted).expect("second format failed");
    assert_eq!(formatted, formatted_again);
}
