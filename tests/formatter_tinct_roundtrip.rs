// Integration test for tinct-hosted formatters (compact and pretty)
// Verifies that formatting is idempotent and parseable

use std::path::PathBuf;
use std::sync::Arc;
use tinct::{format_source_tinct, parse};

fn test_file(src: &str) -> Arc<tinct::ast::SourceFile> {
    Arc::new(tinct::ast::SourceFile {
        path: Arc::from(file!()),
        content: Arc::from(src),
    })
}

// format_source_tinct is async; run it synchronously inside the large-stack thread.
fn fmt_sync(input: &str, script: &std::path::Path) -> Result<String, String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(format_source_tinct(input, script))
}

// format_source_tinct_with_dir triggers deep recursion (macro expansion + AST dict
// conversion) that overflows the 2MB default test-thread stack. Spawn a 32MB thread.
fn run_with_large_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

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
    run_with_large_stack(|| {
        let input = r#"[
  server: [
    port: 8080
    host: "localhost"
  ]
  enabled: true
]"#;

        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        assert!(!formatted.is_empty(), "formatter produced empty output");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again, "formatter is not idempotent");
    });
}

#[test]
fn test_tinct_formatter_pretty_simple_dict() {
    run_with_large_stack(|| {
        let input = r#"[
  server: [
    port: 8080
    host: "localhost"
  ]
  enabled: true
]"#;

        let formatted = fmt_sync(input, &pretty_script()).expect("formatter failed");
        assert!(!formatted.is_empty(), "formatter produced empty output");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again = fmt_sync(&formatted, &pretty_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again, "formatter is not idempotent");
    });
}

#[test]
fn test_tinct_formatter_compact_literals() {
    run_with_large_stack(|| {
        let input = r#"[
  int: 42
  float: 3.14
  bool: true
  str: "hello"
]"#;

        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        let parsed =
            parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        assert_eq!(parsed.program.documents.len(), 1);
        assert_eq!(parsed.program.documents[0].node.expressions().count(), 1);
    });
}

#[test]
fn test_tinct_formatter_pretty_nested_dict() {
    run_with_large_stack(|| {
        let input = r#"[
  outer: [
    inner: [
      deep: 123
    ]
  ]
]"#;

        let formatted = fmt_sync(input, &pretty_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again = fmt_sync(&formatted, &pretty_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_function() {
    run_with_large_stack(|| {
        let input = "[add: [fn [x y] [+ x y]]]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_call() {
    run_with_large_stack(|| {
        let input = "[[fn [x] [+ x 1]] 42]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_empty_dict() {
    run_with_large_stack(|| {
        let input = "[]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        assert_eq!(formatted, "[]\n", "empty dict not formatted correctly");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
    });
}

#[test]
fn test_tinct_formatter_compact_auto_indexed() {
    run_with_large_stack(|| {
        let input = "[1 2 3]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_keyed_entry() {
    run_with_large_stack(|| {
        let input = "[port: 8080]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        assert!(
            formatted.contains("port"),
            "formatted output missing key 'port': {formatted}"
        );
        assert!(
            formatted.contains("8080"),
            "formatted output missing value 8080: {formatted}"
        );
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_multiline_to_oneline() {
    run_with_large_stack(|| {
        let input = "[\n  port: 8080\n  host: \"localhost\"\n]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        let trimmed = formatted.trim();
        assert!(
            !trimmed.contains('\n'),
            "compact formatter should not produce newlines in dict body: {formatted}"
        );
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_pretty_comments_preserved() {
    run_with_large_stack(|| {
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
        let formatted = fmt_sync(input, &pretty_script()).expect("formatter failed");
        assert!(
            formatted.contains("# server configuration"),
            "pretty formatter should preserve comments in block-mode dicts: {formatted}"
        );
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again = fmt_sync(&formatted, &pretty_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_string_quoted() {
    run_with_large_stack(|| {
        let input = r#"[host: "localhost"]"#;
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        assert!(
            formatted.contains('"'),
            "compact formatter should quote string literals: {formatted}"
        );
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
    });
}

#[test]
fn test_tinct_formatter_compact_bool() {
    run_with_large_stack(|| {
        let input = "[enabled: true disabled: false]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        assert!(formatted.contains("true"), "missing 'true': {formatted}");
        assert!(formatted.contains("false"), "missing 'false': {formatted}");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
    });
}

#[test]
fn test_tinct_formatter_compact_match_expr() {
    run_with_large_stack(|| {
        let input = "[result: [match x 1: \"one\" 2: \"two\" _: \"other\"]]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_pretty_multi_document() {
    run_with_large_stack(|| {
        let input = "[x: 1]\n---\n[y: 2]";
        let formatted = fmt_sync(input, &pretty_script()).expect("formatter failed");
        assert!(
            formatted.contains("---"),
            "pretty formatter should preserve document separators: {formatted}"
        );
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again = fmt_sync(&formatted, &pretty_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}

#[test]
fn test_tinct_formatter_compact_multi_document() {
    run_with_large_stack(|| {
        let input = "[x: 1]\n---\n[y: 2]";
        let formatted = fmt_sync(input, &compact_script()).expect("formatter failed");
        parse(&formatted, test_file(&formatted)).expect("formatted output is not parseable");
        let formatted_again =
            fmt_sync(&formatted, &compact_script()).expect("second format failed");
        assert_eq!(formatted, formatted_again);
    });
}
