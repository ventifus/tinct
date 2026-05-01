// Source code pretty-printer. This module reformats LLT source for human reading,
// preserving structure while normalizing whitespace and indentation.
//
// **NOT for error messages.** For rendering an `Expr` node in a diagnostic, use
// `impl Display for Expr` in `src/ast.rs` — that is the normative representation
// for error output. This `Formatter` operates on the raw parse output (token spans)
// to reconstruct well-formatted source, not to describe what an expression means.
use crate::ast::{Annotation, Document, Entry, Expr, NamedArg, Param, Spanned};
use crate::parser::{parse2, ParseError, ParseOutput};

pub fn format_source(input: &str) -> Result<String, ParseError> {
    let output = parse2(input)?;
    let mut formatter = Formatter::new(&output, input);
    formatter.format_file();
    Ok(formatter.output)
}

struct Formatter<'a> {
    parse_output: &'a ParseOutput,
    source: &'a str,
    output: String,
    indent_level: usize,
}

impl<'a> Formatter<'a> {
    fn new(parse_output: &'a ParseOutput, source: &'a str) -> Self {
        Self {
            parse_output,
            source,
            output: String::with_capacity(source.len()),
            indent_level: 0,
        }
    }

    fn format_file(&mut self) {
        let file = &self.parse_output.file;
        for (i, doc) in file.node.documents.iter().enumerate() {
            if i > 0 {
                // Document separator
                if !self.output.is_empty() {
                    if !self.output.ends_with('\n') {
                        self.output.push('\n');
                    }
                    if !self.output.ends_with("\n\n") {
                        self.output.push('\n');
                    }
                }
                self.output.push_str("---");
                self.output.push('\n');
                self.output.push('\n');
            }
            self.format_document(&doc.node, doc.span.start.offset);
        }
        self.ensure_trailing_newline();
    }

    fn format_document(&mut self, document: &Document, _doc_offset: usize) {
        if document.expressions.is_empty() {
            return;
        }

        // Decide if all expressions should be on one line:
        // True if all expressions are simple (not complex Dict/Call/Fn/TypeAlias)
        // AND there are no comments (which would require multi-line formatting)
        let all_simple = document.expressions.iter().all(|e| match &e.node {
            Expr::Dict(entries) if self.is_simple_dict(entries) => true,
            Expr::Dict(_)
            | Expr::Call { .. }
            | Expr::Fn { .. }
            | Expr::TypeAlias(_)
            | Expr::Error(_) => false,
            _ => true,
        });

        let has_comments = document.expressions.iter().any(|e| {
            self.parse_output
                .leading_comments
                .contains_key(&e.span.start.offset)
                || self
                    .parse_output
                    .trailing_comments
                    .contains_key(&e.span.start.offset)
        });

        if all_simple && !has_comments {
            // Format all on one line with spaces
            for (i, expr) in document.expressions.iter().enumerate() {
                if i > 0 {
                    self.output.push(' ');
                }
                self.format_expr_top_level(expr);
            }
        } else {
            // Format each expression on its own line
            for (i, expr) in document.expressions.iter().enumerate() {
                let has_leading_comment = self
                    .parse_output
                    .leading_comments
                    .contains_key(&expr.span.start.offset);

                // Leading comments
                if let Some(comments) = self
                    .parse_output
                    .leading_comments
                    .get(&expr.span.start.offset)
                {
                    if i > 0 {
                        self.output.push('\n');
                    }
                    for comment in comments {
                        self.write_indent();
                        self.output.push('#');
                        self.output.push_str(comment);
                        self.output.push('\n');
                    }
                }

                // Blank line before non-first expression (but not if there's a leading comment)
                if i > 0 && !has_leading_comment {
                    self.output.push('\n');
                }

                self.format_expr_top_level(expr);

                // Trailing comments
                if let Some(comment) = self
                    .parse_output
                    .trailing_comments
                    .get(&expr.span.start.offset)
                {
                    self.output.push(' ');
                    self.output.push('#');
                    self.output.push_str(comment);
                }

                self.output.push('\n');
            }
        }
    }

    fn format_expr_top_level(&mut self, expr: &Spanned<Expr>) {
        self.format_expr(expr, false);
    }

    fn format_expr(&mut self, expr: &Spanned<Expr>, _in_bracket: bool) {
        match &expr.node {
            Expr::Int(n) => self.output.push_str(&n.to_string()),
            Expr::Float(f) => self.output.push_str(&f.to_string()),
            Expr::Bool(b) => self.output.push_str(if *b { "true" } else { "false" }),
            Expr::Str(s) => {
                // Check if this was a quoted string in the original source
                let is_quoted = self
                    .source
                    .as_bytes()
                    .get(expr.span.start.offset)
                    .map_or(false, |&b| b == b'"');
                if is_quoted {
                    self.output.push('"');
                    for ch in s.chars() {
                        match ch {
                            '"' => self.output.push_str("\\\""),
                            '\\' => self.output.push_str("\\\\"),
                            '\n' => self.output.push_str("\\n"),
                            '\t' => self.output.push_str("\\t"),
                            '\r' => self.output.push_str("\\r"),
                            _ => self.output.push(ch),
                        }
                    }
                    self.output.push('"');
                } else {
                    // Bare word
                    self.output.push_str(s);
                }
            }
            Expr::VarRef(name) => {
                self.output.push('$');
                self.output.push_str(name);
            }
            Expr::DotAccess { expr, field } => {
                self.format_expr(expr, false);
                self.output.push('.');
                self.output.push_str(field);
            }
            Expr::BracketAccess { expr, key } => {
                self.format_expr(expr, false);
                self.output.push('[');
                self.format_expr(key, true);
                self.output.push(']');
            }
            Expr::RangeAccess { expr, start, end } => {
                self.format_expr(expr, false);
                self.output.push('[');
                if let Some(s) = start {
                    self.format_expr(s, true);
                }
                self.output.push_str("..");
                if let Some(e) = end {
                    self.format_expr(e, true);
                }
                self.output.push(']');
            }
            Expr::Dict(entries) => self.format_dict(entries),
            Expr::Call {
                func,
                args,
                named_args,
            } => self.format_call(func, args, named_args),
            Expr::Fn {
                return_ann,
                params,
                body,
                desugared: _,
            } => self.format_fn(return_ann, params, body),
            Expr::TypeAlias(type_expr) => {
                self.output.push_str("[type ");
                self.format_expr(type_expr, true);
                self.output.push(']');
            }
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                self.output.push('[');
                self.output.push('@');
                self.format_annotation(annotation);
                self.output.push(' ');
                self.format_expr(expr, true);
                self.output.push(']');
            }
            Expr::Annotated { name, annotation } => {
                self.output.push_str(name);
                self.output.push('@');
                self.format_annotation(annotation);
            }
            Expr::Rest(name) => {
                self.output.push_str("...");
                if let Some(n) = name {
                    self.output.push_str(n);
                }
            }
            Expr::Error(span) => {
                // Emit original source text verbatim for error nodes
                let text = &self.source[span.start.offset..span.end.offset];
                self.output.push_str(text);
            }
        }
    }

    fn format_dict(&mut self, entries: &[Spanned<Entry>]) {
        if entries.is_empty() {
            self.output.push_str("[]");
            return;
        }

        // Decide single-line vs multi-line
        let has_comments = self.dict_has_comments(entries);
        let entry_count = entries.len();
        let single_line_width = self.measure_dict_width(entries);

        let use_single_line = if has_comments {
            false
        } else {
            entry_count <= 4 && single_line_width <= 80
        };

        if use_single_line {
            self.format_dict_single_line(entries);
        } else {
            self.format_dict_multi_line(entries);
        }
    }

    fn is_simple_dict(&self, entries: &[Spanned<Entry>]) -> bool {
        // A simple dict is one that would fit on a single line (4 or fewer entries, width <= 80)
        let has_comments = self.dict_has_comments(entries);
        let entry_count = entries.len();
        let single_line_width = self.measure_dict_width(entries);
        !has_comments && entry_count <= 4 && single_line_width <= 80
    }

    fn dict_has_comments(&self, entries: &[Spanned<Entry>]) -> bool {
        for entry in entries {
            // Check for leading comments at the entry start
            if self
                .parse_output
                .leading_comments
                .contains_key(&entry.span.start.offset)
            {
                return true;
            }
            // Check for trailing comments at the value end (parser keys them by the last token)
            if self
                .parse_output
                .trailing_comments
                .contains_key(&entry.node.value.span.start.offset)
            {
                return true;
            }
        }
        false
    }

    fn measure_dict_width(&self, entries: &[Spanned<Entry>]) -> usize {
        let mut width = 2; // [ and ]
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                width += 1; // space
            }
            if let Some(key) = &entry.node.key {
                width += self.measure_expr_width(&key.node);
                width += 2; // ": "
            }
            width += self.measure_expr_width(&entry.node.value.node);
        }
        width + self.indent_level * 2
    }

    fn measure_expr_width(&self, expr: &Expr) -> usize {
        match expr {
            Expr::Int(n) => {
                if *n < 0 {
                    1 + (-n).to_string().len()
                } else {
                    n.to_string().len()
                }
            }
            Expr::Float(f) => f.to_string().len(),
            Expr::Bool(b) => {
                if *b {
                    4
                } else {
                    5
                }
            }
            Expr::Str(s) => {
                // Check if it's quoted in the source
                // For width measurement, we approximate
                s.len() + 2 // conservative estimate
            }
            Expr::VarRef(name) => name.len() + 1,
            Expr::DotAccess { expr, field } => {
                self.measure_expr_width(&expr.node) + 1 + field.len()
            }
            Expr::BracketAccess { expr, key } => {
                self.measure_expr_width(&expr.node) + 1 + self.measure_expr_width(&key.node) + 1
            }
            Expr::RangeAccess { expr, start, end } => {
                let mut w = self.measure_expr_width(&expr.node) + 3; // [..]
                if let Some(s) = start {
                    w += self.measure_expr_width(&s.node);
                }
                if let Some(e) = end {
                    w += self.measure_expr_width(&e.node);
                }
                w
            }
            Expr::Dict(entries) => self.measure_dict_width(entries),
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                let mut w = 1 + 4 + 1; // [call ]
                w += self.measure_expr_width(&func.node);
                for arg in args {
                    w += 1 + self.measure_expr_width(&arg.node);
                }
                for named_arg in named_args {
                    w += 1
                        + named_arg.node.name.len()
                        + 2
                        + self.measure_expr_width(&named_arg.node.value.node);
                }
                w + 1 // ]
            }
            Expr::Fn { params, body, .. } => {
                let mut w = 1 + 2 + 1; // [fn ]
                w += 1; // [
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        w += 1;
                    }
                    w += param.node.name.len();
                    if param.node.variadic {
                        w += 3; // ...
                    }
                    if let Some(ann) = &param.node.annotation {
                        w += 1 + self.measure_annotation_width(&ann.node);
                    }
                }
                w += 1; // ]
                w += 1; // space
                w += self.measure_expr_width(&body.node);
                w + 1 // ]
            }
            Expr::TypeAlias(type_expr) => 1 + 4 + 1 + self.measure_expr_width(&type_expr.node) + 1,
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                1 + 1
                    + self.measure_annotation_width(&annotation.node)
                    + 1
                    + self.measure_expr_width(&expr.node)
                    + 1
            }
            Expr::Annotated { name, annotation } => {
                name.len() + 1 + self.measure_annotation_width(&annotation.node)
            }
            Expr::Rest(name) => 3 + name.as_ref().map_or(0, |n| n.len()),
            Expr::Error(span) => {
                // Measure the width of the original source text
                span.end.offset - span.start.offset
            }
        }
    }

    fn measure_annotation_width(&self, annotation: &Annotation) -> usize {
        match annotation {
            Annotation::Simple(name) => name.len(),
            Annotation::PropertyDict(entries) => self.measure_dict_width(entries),
        }
    }

    fn format_dict_single_line(&mut self, entries: &[Spanned<Entry>]) {
        self.output.push('[');
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                self.output.push(' ');
            }
            if let Some(key) = &entry.node.key {
                self.format_expr(key, true);
                self.output.push_str(": ");
            }
            self.format_expr(&entry.node.value, true);
        }
        self.output.push(']');
    }

    fn format_dict_multi_line(&mut self, entries: &[Spanned<Entry>]) {
        self.output.push('[');
        self.indent_level += 1;

        for entry in entries {
            // Leading comments for this entry
            if let Some(comments) = self
                .parse_output
                .leading_comments
                .get(&entry.span.start.offset)
            {
                for comment in comments {
                    self.output.push('\n');
                    self.write_indent();
                    self.output.push('#');
                    self.output.push_str(comment);
                }
            }

            self.output.push('\n');
            self.write_indent();
            if let Some(key) = &entry.node.key {
                self.format_expr(key, true);
                self.output.push_str(": ");
            }
            self.format_expr(&entry.node.value, true);

            // Trailing comments for this entry (keyed by value offset)
            if let Some(comment) = self
                .parse_output
                .trailing_comments
                .get(&entry.node.value.span.start.offset)
            {
                self.output.push(' ');
                self.output.push('#');
                self.output.push_str(comment);
            }
        }

        self.indent_level -= 1;
        self.output.push('\n');
        self.write_indent();
        self.output.push(']');
    }

    fn format_call(
        &mut self,
        func: &Spanned<Expr>,
        args: &[Spanned<Expr>],
        named_args: &[Spanned<NamedArg>],
    ) {
        self.output.push_str("[call ");
        self.format_expr(func, true);
        for arg in args {
            self.output.push(' ');
            self.format_expr(arg, true);
        }
        for named_arg in named_args {
            self.output.push(' ');
            self.output.push_str(&named_arg.node.name);
            self.output.push_str(": ");
            self.format_expr(&named_arg.node.value, true);
        }
        self.output.push(']');
    }

    fn format_fn(
        &mut self,
        return_ann: &Option<Spanned<Annotation>>,
        params: &[Spanned<Param>],
        body: &Spanned<Expr>,
    ) {
        self.output.push('[');
        self.output.push_str("fn");
        if let Some(ann) = return_ann {
            self.output.push('@');
            self.format_annotation(ann);
        }
        self.output.push(' ');

        // Params always single-line
        self.output.push('[');
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                self.output.push(' ');
            }
            if param.node.variadic {
                self.output.push_str("...");
            }
            self.output.push_str(&param.node.name);
            if let Some(ann) = &param.node.annotation {
                self.output.push('@');
                self.format_annotation(ann);
            }
        }
        self.output.push(']');

        self.output.push(' ');
        self.format_expr(body, true);
        self.output.push(']');
    }

    fn format_annotation(&mut self, annotation: &Spanned<Annotation>) {
        match &annotation.node {
            Annotation::Simple(name) => self.output.push_str(name),
            Annotation::PropertyDict(entries) => {
                // Format as a dict bracket
                self.format_dict(entries);
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("  ");
        }
    }

    fn ensure_trailing_newline(&mut self) {
        if !self.output.ends_with('\n') {
            self.output.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert_eq!(format_source("").unwrap(), "\n");
    }

    #[test]
    fn test_empty_brackets() {
        assert_eq!(format_source("[]").unwrap(), "[]\n");
    }

    #[test]
    fn test_simple_dict() {
        assert_eq!(format_source("[x: 1]").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_semicolons_removed() {
        assert_eq!(format_source("[x: 1; y: 2]").unwrap(), "[x: 1 y: 2]\n");
    }

    #[test]
    fn test_key_value_spacing() {
        assert_eq!(format_source("[x:1]").unwrap(), "[x: 1]\n");
        assert_eq!(format_source("[x :  1]").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_single_line_four_entries() {
        let input = "[a: 1 b: 2 c: 3 d: 4]";
        assert_eq!(format_source(input).unwrap(), "[a: 1 b: 2 c: 3 d: 4]\n");
    }

    #[test]
    fn test_multi_line_five_entries() {
        let input = "[a: 1 b: 2 c: 3 d: 4 e: 5]";
        let expected = "[\n  a: 1\n  b: 2\n  c: 3\n  d: 4\n  e: 5\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_multi_line_width_exceeded() {
        let long_value = "a".repeat(76);
        let input = format!("[x: {long_value}]");
        let expected = format!("[\n  x: {long_value}\n]\n");
        assert_eq!(format_source(&input).unwrap(), expected);
    }

    #[test]
    fn test_indentation_nested() {
        let input = "[outer: [inner: [deep: 1]]]";
        assert_eq!(
            format_source(input).unwrap(),
            "[outer: [inner: [deep: 1]]]\n"
        );
    }

    #[test]
    fn test_indentation_nested_multi_line() {
        let input = "[a: 1 b: 2 c: [x: 1 y: 2] d: 3 e: 4]";
        let expected = "[\n  a: 1\n  b: 2\n  c: [x: 1 y: 2]\n  d: 3\n  e: 4\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_trailing_comment() {
        let input = "[x: 1 # this is x\ny: 2]";
        let expected = "[\n  x: 1 # this is x\n  y: 2\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_leading_comment() {
        let input = "# header comment\n[x: 1]";
        assert_eq!(format_source(input).unwrap(), "# header comment\n[x: 1]\n");
    }

    #[test]
    fn test_section_comment() {
        let input = "[a: 1]\n\n# Section B\n[b: 2]";
        assert_eq!(
            format_source(input).unwrap(),
            "[a: 1]\n\n# Section B\n[b: 2]\n"
        );
    }

    #[test]
    fn test_blank_line_collapsing() {
        let input = "[x: 1]\n\n\n\n[y: 2]";
        // AST-based formatter normalizes formatting - simple dicts on one line
        assert_eq!(format_source(input).unwrap(), "[x: 1] [y: 2]\n");
    }

    #[test]
    fn test_doc_separator() {
        let input = "[x: 1]\n---\n[y: 2]";
        assert_eq!(format_source(input).unwrap(), "[x: 1]\n\n---\n\n[y: 2]\n");
    }

    #[test]
    fn test_doc_separator_first_document() {
        let input = "[x: 1]\n---\n[y: 2]";
        let expected = "[x: 1]\n\n---\n\n[y: 2]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_trailing_newline() {
        assert_eq!(format_source("[x: 1]").unwrap(), "[x: 1]\n");
        assert_eq!(format_source("[x: 1]\n\n\n").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_access_chain() {
        assert_eq!(format_source("$a.b.c").unwrap(), "$a.b.c\n");
    }

    #[test]
    fn test_access_chain_in_dict() {
        assert_eq!(format_source("[x: $a.b[0].c]").unwrap(), "[x: $a.b[0].c]\n");
    }

    #[test]
    fn test_bracket_access_spacing() {
        // No whitespace before [ — stays without space (BracketAccess token)
        assert_eq!(format_source("$a[0]").unwrap(), "$a[0]\n");
        // Whitespace before [ — space preserved (OpenBracket token, separate expression)
        assert_eq!(format_source("$a [0]").unwrap(), "$a [0]\n");
    }

    #[test]
    fn test_immediate_at_spacing() {
        // ImmediateAt in type-assert context: no space before @ — stays without space
        assert_eq!(format_source("[@Int 42]").unwrap(), "[@Int 42]\n");
        // Annotation in param context
        assert_eq!(
            format_source("[fn [x@Int] $x]").unwrap(),
            "[fn [x@Int] $x]\n"
        );
    }

    #[test]
    fn test_annotation_no_spaces() {
        // Annotations in type-assert context
        assert_eq!(format_source("[@Number 42]").unwrap(), "[@Number 42]\n");
        assert_eq!(format_source("[@ Number 42]").unwrap(), "[@Number 42]\n");
    }

    #[test]
    fn test_quoted_string_preserved() {
        let input = r#"[msg: "hello\nworld"]"#;
        assert_eq!(format_source(input).unwrap(), "[msg: \"hello\\nworld\"]\n");
    }

    #[test]
    fn test_idempotency() {
        let input = "[a: 1 b: 2 c: 3 d: 4]";
        let formatted = format_source(input).unwrap();
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(formatted, reformatted);
    }

    #[test]
    fn test_idempotency_multi_line() {
        let input = "[\n  a: 1\n  b: 2\n  c: 3\n  d: 4\n  e: 5\n]\n";
        let reformatted = format_source(input).unwrap();
        assert_eq!(input, reformatted);
    }

    #[test]
    fn test_variadic_rest() {
        assert_eq!(format_source("[... x]").unwrap(), "[...x]\n");
        assert_eq!(format_source("[...rest]").unwrap(), "[...rest]\n");
    }

    #[test]
    fn test_range_operator() {
        // Range operator in bracket access context
        assert_eq!(format_source("$x[0..10]").unwrap(), "$x[0..10]\n");
    }

    #[test]
    fn test_positional_entries() {
        assert_eq!(format_source("[1 2 3]").unwrap(), "[1 2 3]\n");
    }

    #[test]
    fn test_deeply_nested() {
        let input = "[a: [b: [c: [d: [e: 1]]]]]";
        assert_eq!(
            format_source(input).unwrap(),
            "[a: [b: [c: [d: [e: 1]]]]]\n"
        );
    }

    #[test]
    fn test_fn_params_always_single_line() {
        let input = "[fn [param1 param2 param3 param4 param5] [x: 1]]";
        let formatted = format_source(input).unwrap();
        assert!(formatted.contains("[param1 param2 param3 param4 param5]"));
    }

    #[test]
    fn test_boolean_literals() {
        assert_eq!(
            format_source("[x: true y: false]").unwrap(),
            "[x: true y: false]\n"
        );
    }

    #[test]
    fn test_negative_numbers() {
        assert_eq!(format_source("[-1 -2.5]").unwrap(), "[-1 -2.5]\n");
    }

    #[test]
    fn test_var_refs() {
        assert_eq!(format_source("[$x $y]").unwrap(), "[$x $y]\n");
        assert_eq!(format_source("[$$]").unwrap(), "[$$]\n");
    }

    #[test]
    fn test_complex_real_world() {
        let input = r#"
[
  name: "test"
  version: 1
  deps: [lodash react vue]
  config: [
    timeout: 30
    retries: 3
  ]
]
"#;
        let formatted = format_source(input).unwrap();
        assert!(formatted.contains("[\n  name: \"test\""));
        assert!(formatted.contains("  deps: [lodash react vue]"));
        assert!(formatted.contains("  config: [timeout: 30 retries: 3]"));
    }

    #[test]
    fn test_error_invalid_input() {
        let result = format_source("[unterminated string \"hello");
        assert!(result.is_err());
    }

    #[test]
    fn test_call_expression_single_line() {
        let input = "[call $func arg1 arg2 arg3]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[call $func arg1 arg2 arg3]\n");
    }

    #[test]
    fn test_call_expression_many_args_single_line() {
        let input = "[call $if $cond $then $else]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[call $if $cond $then $else]\n");
    }

    #[test]
    fn test_fn_definition_single_line() {
        let input = "[fn [x y] [call $add $x $y]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[fn [x y] [call $add $x $y]]\n");
    }

    #[test]
    fn test_comment_forces_multi_line() {
        let input = "[a: 1 # comment\nb: 2]";
        let formatted = format_source(input).unwrap();
        assert!(formatted.starts_with("[\n"));
        assert!(formatted.contains("  a: 1 # comment"));
        assert!(formatted.contains("  b: 2"));
    }

    #[test]
    fn test_top_level_spacing_preserved() {
        assert_eq!(format_source("$a $b").unwrap(), "$a $b\n");
    }

    #[test]
    fn test_top_level_bracket_not_merged() {
        let input = "$a [x: 1]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "$a [x: 1]\n");
    }

    #[test]
    fn test_top_level_access_chain() {
        assert_eq!(format_source("$a.b.c").unwrap(), "$a.b.c\n");
    }

    #[test]
    fn test_top_level_annotation() {
        // Type-assert at top level
        assert_eq!(format_source("[@Number 42]").unwrap(), "[@Number 42]\n");
    }

    #[test]
    fn test_annotated_key() {
        // Annotated param in function
        assert_eq!(
            format_source("[fn [x@Int] $x]").unwrap(),
            "[fn [x@Int] $x]\n"
        );
    }

    #[test]
    fn test_formatter_error_unterminated_string() {
        let result = format_source("[\"unterminated");
        assert!(
            result.is_err(),
            "unterminated string should produce a lex error"
        );
    }

    #[test]
    fn test_formatter_error_bare_dollar() {
        let result = format_source("$");
        assert!(
            result.is_err(),
            "bare $ without identifier should produce a lex error"
        );
    }

    #[test]
    fn test_formatter_error_invalid_escape() {
        let result = format_source(r#""\q""#);
        assert!(
            result.is_err(),
            "invalid escape sequence \\q should produce a lex error"
        );
    }
}
