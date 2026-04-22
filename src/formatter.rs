use crate::ast::Spanned;
use crate::lexer::{tokenize, LexError, Token};

pub fn format_source(input: &str) -> Result<String, LexError> {
    let tokens = tokenize(input)?;
    let formatter = Formatter::new(&tokens);
    Ok(formatter.format())
}

struct Formatter<'a> {
    tokens: &'a [Spanned<Token>],
    output: String,
    indent_level: usize,
}

impl<'a> Formatter<'a> {
    fn new(tokens: &'a [Spanned<Token>]) -> Self {
        Self {
            tokens,
            output: String::with_capacity(tokens.len() * 10),
            indent_level: 0,
        }
    }

    fn format(mut self) -> String {
        self.format_tokens(0, self.tokens.len());
        self.ensure_trailing_newline();
        self.output
    }

    fn format_tokens(&mut self, start: usize, end: usize) {
        let mut i = start;
        let mut at_line_start = true;
        let mut blank_line_pending = false;
        let mut needs_space = false;

        while i < end {
            let token = &self.tokens[i].node;

            match token {
                Token::Newline => {
                    if !at_line_start {
                        self.output.push('\n');
                        at_line_start = true;
                    }
                    let mut extra = 0;
                    while i + 1 < end && matches!(self.tokens[i + 1].node, Token::Newline) {
                        i += 1;
                        extra += 1;
                    }
                    if extra > 0 {
                        blank_line_pending = true;
                    }
                    needs_space = false;
                    i += 1;
                }

                Token::Comment(text) => {
                    if !at_line_start {
                        self.output.push(' ');
                    } else {
                        if blank_line_pending {
                            self.output.push('\n');
                            blank_line_pending = false;
                        }
                        self.write_indent();
                    }
                    self.output.push('#');
                    self.output.push_str(text);
                    self.output.push('\n');
                    at_line_start = true;
                    needs_space = false;
                    i += 1;
                    if i < end && matches!(self.tokens[i].node, Token::Newline) {
                        i += 1;
                    }
                }

                Token::DocSeparator => {
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
                    at_line_start = true;
                    blank_line_pending = false;
                    needs_space = false;
                    i += 1;
                }

                Token::OpenBracket => {
                    if let Some(close_idx) = self.find_matching_bracket(i) {
                        if blank_line_pending {
                            self.output.push('\n');
                            blank_line_pending = false;
                        }
                        if at_line_start {
                            self.write_indent();
                        } else {
                            let is_index = i > 0
                                && !self.has_whitespace_between(i - 1, i)
                                && !matches!(
                                    self.tokens[i - 1].node,
                                    Token::OpenBracket | Token::Colon | Token::Semicolon
                                );
                            if needs_space && !is_index {
                                self.output.push(' ');
                            }
                        }
                        self.format_bracket_expr(i, close_idx);
                        i = close_idx + 1;
                        at_line_start = false;
                        needs_space = true;
                    } else {
                        if at_line_start {
                            self.write_indent();
                        }
                        self.output.push('[');
                        i += 1;
                        at_line_start = false;
                        needs_space = true;
                    }
                }

                Token::Semicolon => {
                    i += 1;
                }

                Token::Dot => {
                    if at_line_start {
                        if blank_line_pending {
                            self.output.push('\n');
                            blank_line_pending = false;
                        }
                        self.write_indent();
                    }
                    self.output.push('.');
                    at_line_start = false;
                    needs_space = false;
                    i += 1;
                }

                Token::At => {
                    if at_line_start {
                        if blank_line_pending {
                            self.output.push('\n');
                            blank_line_pending = false;
                        }
                        self.write_indent();
                    }
                    self.output.push('@');
                    at_line_start = false;
                    needs_space = false;
                    i += 1;
                }

                Token::Range => {
                    self.output.push_str("..");
                    at_line_start = false;
                    needs_space = false;
                    i += 1;
                }

                _ => {
                    if at_line_start {
                        if blank_line_pending {
                            self.output.push('\n');
                            blank_line_pending = false;
                        }
                        self.write_indent();
                    } else if needs_space {
                        self.output.push(' ');
                    }
                    self.write_token(token);
                    at_line_start = false;
                    needs_space = !matches!(token, Token::At);
                    i += 1;
                }
            }
        }
    }

    fn find_matching_bracket(&self, open_idx: usize) -> Option<usize> {
        let mut depth = 0;
        for i in open_idx..self.tokens.len() {
            match &self.tokens[i].node {
                Token::OpenBracket => depth += 1,
                Token::CloseBracket => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn has_whitespace_between(&self, a: usize, b: usize) -> bool {
        self.tokens[a].span.end.offset < self.tokens[b].span.start.offset
    }

    fn bracket_has_comments(&self, open_idx: usize, close_idx: usize) -> bool {
        for i in (open_idx + 1)..close_idx {
            if matches!(self.tokens[i].node, Token::Comment(_)) {
                return true;
            }
        }
        false
    }

    fn format_bracket_expr(&mut self, open_idx: usize, close_idx: usize) {
        let entry_count = self.count_entries(open_idx + 1, close_idx);
        let single_line_width = self.measure_single_line_width(open_idx, close_idx);
        let is_fn_params = self.is_fn_params(open_idx, close_idx);
        let has_comments = self.bracket_has_comments(open_idx, close_idx);

        let use_single_line = if is_fn_params {
            true
        } else if has_comments {
            false
        } else {
            entry_count <= 4 && single_line_width <= 80
        };

        if use_single_line {
            self.format_bracket_single_line(open_idx, close_idx);
        } else {
            self.format_bracket_multi_line(open_idx, close_idx);
        }
    }

    fn count_entries(&self, start: usize, end: usize) -> usize {
        let mut count = 0;
        let mut i = start;
        let mut after_colon = false;
        let mut after_dot = false;
        let mut after_at = false;
        let mut after_ellipsis = false;
        let mut first_positional = true;

        while i < end {
            match &self.tokens[i].node {
                Token::Newline | Token::Semicolon | Token::Comment(_) => {
                    i += 1;
                    continue;
                }
                Token::OpenBracket => {
                    let is_cont = after_colon || after_dot || after_at || after_ellipsis;
                    let is_index = i > start
                        && !self.has_whitespace_between(i - 1, i)
                        && !matches!(
                            self.tokens[i - 1].node,
                            Token::OpenBracket | Token::Colon | Token::Semicolon
                        );
                    if !is_cont && !is_index {
                        count += 1;
                    }
                    if let Some(close) = self.find_matching_bracket(i) {
                        i = close + 1;
                    } else {
                        i += 1;
                    }
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                    first_positional = false;
                    continue;
                }
                Token::Colon => {
                    count += 1;
                    after_colon = true;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                    first_positional = false;
                }
                Token::Dot => {
                    after_dot = true;
                    after_colon = false;
                    after_at = false;
                }
                Token::At => {
                    after_at = true;
                    after_colon = false;
                    after_dot = false;
                }
                Token::Range => {
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                }
                Token::Ellipsis => {
                    let is_cont = after_colon || after_dot;
                    if !is_cont {
                        count += 1;
                    }
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = i + 1 < end
                        && !self.has_whitespace_between(i, i + 1)
                        && !matches!(
                            self.tokens[i + 1].node,
                            Token::Newline | Token::CloseBracket | Token::Semicolon
                        );
                    first_positional = false;
                }
                _ => {
                    let is_cont = after_colon || after_dot || after_at || after_ellipsis;
                    if !is_cont {
                        let is_key = self.is_followed_by_colon(i, end);
                        if !is_key {
                            let skip = first_positional
                                && matches!(
                                    &self.tokens[i].node,
                                    Token::BareWord(s) if s == "call" || s == "fn" || s == "type"
                                );
                            if !skip {
                                count += 1;
                            }
                        }
                    }
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                    first_positional = false;
                }
            }
            i += 1;
        }

        count
    }

    fn is_followed_by_colon(&self, idx: usize, end: usize) -> bool {
        let mut j = idx + 1;
        while j < end {
            match &self.tokens[j].node {
                Token::Colon => return true,
                Token::Comment(_) | Token::Dot => {
                    j += 1;
                }
                Token::At => {
                    j += 1;
                    if j < end && matches!(self.tokens[j].node, Token::BareWord(_)) {
                        j += 1;
                    }
                }
                _ => return false,
            }
        }
        false
    }

    fn measure_single_line_width(&self, open_idx: usize, close_idx: usize) -> usize {
        let mut width = 2; // [ and ]
        let mut i = open_idx + 1;
        let mut needs_space = false;

        while i < close_idx {
            match &self.tokens[i].node {
                Token::Newline | Token::Comment(_) | Token::Semicolon => {}
                Token::Colon => {
                    width += 1;
                    needs_space = true;
                }
                Token::Range => {
                    width += 2;
                    needs_space = false;
                }
                Token::Ellipsis => {
                    if needs_space {
                        width += 1;
                    }
                    width += 3;
                    if i + 1 < close_idx
                        && !self.has_whitespace_between(i, i + 1)
                        && !matches!(
                            self.tokens[i + 1].node,
                            Token::Newline | Token::CloseBracket | Token::Semicolon
                        )
                    {
                        needs_space = false;
                    } else {
                        needs_space = true;
                    }
                }
                Token::OpenBracket => {
                    let is_index = i > open_idx + 1
                        && !self.has_whitespace_between(i - 1, i)
                        && !matches!(
                            self.tokens[i - 1].node,
                            Token::OpenBracket | Token::Colon | Token::Semicolon
                        );
                    if needs_space && !is_index {
                        width += 1;
                    }
                    if let Some(nested_close) = self.find_matching_bracket(i) {
                        width += self.measure_single_line_width(i, nested_close);
                        i = nested_close + 1;
                        needs_space = true;
                        continue;
                    } else {
                        width += 1;
                        needs_space = true;
                    }
                }
                token => {
                    if needs_space {
                        width += 1;
                    }
                    width += self.token_width(token);
                    needs_space = !matches!(token, Token::At | Token::Dot);
                }
            }
            i += 1;
        }

        width + self.indent_level * 2
    }

    fn token_width(&self, token: &Token) -> usize {
        match token {
            Token::OpenBracket | Token::CloseBracket => 1,
            Token::Colon | Token::Semicolon | Token::Dot | Token::At => 1,
            Token::Range => 2,
            Token::Ellipsis => 3,
            Token::DocSeparator => 3,
            Token::Newline => 0,
            Token::Comment(s) => 1 + s.len(),
            Token::Int(n) => {
                if *n < 0 {
                    1 + (-n).to_string().len()
                } else {
                    n.to_string().len()
                }
            }
            Token::Float(f) => f.to_string().len(),
            Token::BareWord(s) => s.len(),
            Token::QuotedString(s) => s.len() + 2,
            Token::VarRef(s) => s.len() + 1,
            Token::BoolLit(b) => {
                if *b {
                    4
                } else {
                    5
                }
            }
        }
    }

    fn is_fn_params(&self, open_idx: usize, _close_idx: usize) -> bool {
        if open_idx < 2 {
            return false;
        }
        for i in (0..open_idx).rev() {
            match &self.tokens[i].node {
                Token::BareWord(s) if s == "fn" => return true,
                Token::Newline | Token::Comment(_) => continue,
                _ => break,
            }
        }
        false
    }

    fn format_bracket_single_line(&mut self, open_idx: usize, close_idx: usize) {
        self.output.push('[');
        let mut i = open_idx + 1;
        let mut needs_space = false;

        while i < close_idx {
            match &self.tokens[i].node {
                Token::Newline | Token::Comment(_) | Token::Semicolon => {
                    i += 1;
                    continue;
                }
                Token::Colon => {
                    self.output.push(':');
                    self.output.push(' ');
                    needs_space = false;
                }
                Token::At => {
                    self.output.push('@');
                    needs_space = false;
                }
                Token::Dot => {
                    self.output.push('.');
                    needs_space = false;
                }
                Token::Range => {
                    self.output.push_str("..");
                    needs_space = false;
                }
                Token::Ellipsis => {
                    if needs_space {
                        self.output.push(' ');
                    }
                    self.output.push_str("...");
                    if i + 1 < close_idx
                        && !self.has_whitespace_between(i, i + 1)
                        && !matches!(
                            self.tokens[i + 1].node,
                            Token::Newline | Token::CloseBracket | Token::Semicolon
                        )
                    {
                        needs_space = false;
                    } else {
                        needs_space = true;
                    }
                }
                Token::OpenBracket => {
                    let is_index = i > 0
                        && !self.has_whitespace_between(i - 1, i)
                        && !matches!(
                            self.tokens[i - 1].node,
                            Token::OpenBracket | Token::Colon | Token::Semicolon
                        );
                    if needs_space && !is_index {
                        self.output.push(' ');
                    }
                    if let Some(nested_close) = self.find_matching_bracket(i) {
                        self.format_bracket_expr(i, nested_close);
                        i = nested_close + 1;
                        needs_space = true;
                        continue;
                    } else {
                        self.output.push('[');
                        needs_space = true;
                    }
                }
                token => {
                    if needs_space {
                        self.output.push(' ');
                    }
                    self.write_token(token);
                    needs_space = !matches!(token, Token::At);
                }
            }
            i += 1;
        }

        self.output.push(']');
    }

    fn format_bracket_multi_line(&mut self, open_idx: usize, close_idx: usize) {
        self.output.push('[');
        self.indent_level += 1;

        let mut i = open_idx + 1;
        let mut in_entry = false;
        let mut after_colon = false;
        let mut after_dot = false;
        let mut after_at = false;
        let mut after_ellipsis = false;

        while i < close_idx {
            match &self.tokens[i].node {
                Token::Newline | Token::Semicolon => {
                    i += 1;
                    continue;
                }
                Token::Comment(text) => {
                    if in_entry {
                        self.output.push(' ');
                    } else {
                        self.output.push('\n');
                        self.write_indent();
                    }
                    self.output.push('#');
                    self.output.push_str(text);
                    in_entry = false;
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                    i += 1;
                    continue;
                }
                Token::Colon => {
                    self.output.push(':');
                    self.output.push(' ');
                    after_colon = true;
                    after_dot = false;
                    after_at = false;
                    i += 1;
                    continue;
                }
                Token::Dot => {
                    self.output.push('.');
                    after_dot = true;
                    after_colon = false;
                    after_at = false;
                    i += 1;
                    continue;
                }
                Token::At => {
                    self.output.push('@');
                    after_at = true;
                    after_colon = false;
                    after_dot = false;
                    i += 1;
                    continue;
                }
                Token::Range => {
                    self.output.push_str("..");
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    i += 1;
                    continue;
                }
                Token::Ellipsis => {
                    let is_continuation = after_colon || after_dot;
                    if !is_continuation {
                        self.output.push('\n');
                        self.write_indent();
                    }
                    self.output.push_str("...");
                    in_entry = true;
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    if i + 1 < close_idx
                        && !self.has_whitespace_between(i, i + 1)
                        && !matches!(
                            self.tokens[i + 1].node,
                            Token::Newline | Token::CloseBracket | Token::Semicolon
                        )
                    {
                        after_ellipsis = true;
                    } else {
                        after_ellipsis = false;
                    }
                    i += 1;
                    continue;
                }
                Token::OpenBracket => {
                    if let Some(nested_close) = self.find_matching_bracket(i) {
                        let is_continuation =
                            after_colon || after_dot || after_at || after_ellipsis;
                        let is_index = !self.has_whitespace_between(i - 1, i)
                            && !matches!(
                                self.tokens[i - 1].node,
                                Token::OpenBracket | Token::Colon | Token::Semicolon
                            );

                        if !is_continuation && !is_index {
                            self.output.push('\n');
                            self.write_indent();
                        }

                        self.format_bracket_expr(i, nested_close);
                        i = nested_close + 1;
                        in_entry = true;
                        after_colon = false;
                        after_dot = false;
                        after_at = false;
                        after_ellipsis = false;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                token => {
                    let is_continuation = after_colon || after_dot || after_at || after_ellipsis;

                    if !is_continuation {
                        self.output.push('\n');
                        self.write_indent();
                    }

                    self.write_token(token);
                    in_entry = true;
                    after_colon = false;
                    after_dot = false;
                    after_at = false;
                    after_ellipsis = false;
                    i += 1;
                    continue;
                }
            }
        }

        self.indent_level -= 1;
        self.output.push('\n');
        self.write_indent();
        self.output.push(']');
    }

    fn write_token(&mut self, token: &Token) {
        match token {
            Token::OpenBracket => self.output.push('['),
            Token::CloseBracket => self.output.push(']'),
            Token::Colon => self.output.push(':'),
            Token::Semicolon => self.output.push(';'),
            Token::Dot => self.output.push('.'),
            Token::Range => self.output.push_str(".."),
            Token::At => self.output.push('@'),
            Token::Ellipsis => self.output.push_str("..."),
            Token::DocSeparator => self.output.push_str("---"),
            Token::Newline => {}
            Token::Comment(text) => {
                self.output.push('#');
                self.output.push_str(text);
            }
            Token::Int(n) => self.output.push_str(&n.to_string()),
            Token::Float(f) => self.output.push_str(&f.to_string()),
            Token::BareWord(s) => self.output.push_str(s),
            Token::QuotedString(s) => {
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
            }
            Token::VarRef(name) => {
                self.output.push('$');
                self.output.push_str(name);
            }
            Token::BoolLit(b) => self.output.push_str(if *b { "true" } else { "false" }),
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
        assert_eq!(format_source(input).unwrap(), "[x: 1]\n\n[y: 2]\n");
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
    fn test_annotation_no_spaces() {
        assert_eq!(format_source("x@Number").unwrap(), "x@Number\n");
        assert_eq!(format_source("x @ Number").unwrap(), "x@Number\n");
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
        assert_eq!(format_source("[... x]").unwrap(), "[... x]\n");
        assert_eq!(format_source("[...rest]").unwrap(), "[...rest]\n");
    }

    #[test]
    fn test_range_operator() {
        assert_eq!(format_source("[0..10]").unwrap(), "[0..10]\n");
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
        assert_eq!(format_source("x@Number").unwrap(), "x@Number\n");
    }

    #[test]
    fn test_annotated_key() {
        assert_eq!(format_source("[x@Int: 1]").unwrap(), "[x@Int: 1]\n");
    }
}
