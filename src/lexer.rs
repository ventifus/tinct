//! Hand-written tokenizer for tinct.
//!
//! Produces a flat token stream with accurate source spans. Used by the formatter
//! and eventually by an iterative parser.
//!
//! See doc/02-syntax.md §Tokenization Rules for the full specification.

use std::fmt;

use crate::ast::{Position, Span, Spanned};

/// Token types for the tinct lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `[` (opening bracket)
    OpenBracket,
    /// `]` (closing bracket)
    CloseBracket,
    /// `:` (key-value separator)
    Colon,
    /// `;` (newline alias — equivalent to `\n` in all parse positions)
    Semicolon,
    /// `.` (dot access operator, only in access context)
    Dot,
    /// `|` (pipe operator)
    Pipe,
    /// `@` (annotation separator)
    At,
    /// `@` immediately after a bare word (no whitespace gap) — annotation
    ImmediateAt,
    /// `...` (variadic/rest marker)
    Ellipsis,
    /// `---` (document separator)
    DocSeparator,
    /// Significant newline (preserves vertical spacing)
    Newline,
    /// Comment from `#` to EOL (preserves text for formatter)
    Comment(String),
    /// Integer literal
    Int(i64),
    /// Float literal
    Float(f64),
    /// Identifier (bare word — variable reference in value position)
    Identifier(String),
    /// Quoted string literal (escapes already processed)
    QuotedString(String),
    /// Escaped reference `$name` (disambiguator in head/key positions)
    EscapedRef(String),
    /// Boolean literal (`true` or `false`)
    BoolLit(bool),
    /// Interpolated string `i"..."` with parts: literals and variable references
    InterpolatedString(Vec<InterpolatedPart>),
    /// Triple-quoted string `"""..."""` (raw content, no escape processing except `\"\"\"`)
    TripleQuotedString(String),
    /// Triple-quoted interpolated string `i"""..."""` with parts
    TripleInterpolatedString(Vec<InterpolatedPart>),
    /// Reserved keyword `let` — binding declaration
    Let,
    /// Reserved keyword `case` — match arm with explicit scoping
    Case,
}

/// Parts of an interpolated string.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpolatedPart {
    /// Literal text segment
    Literal(String),
    /// Variable reference `$name`
    VarRef(String),
    /// Expression interpolation `${expr}` — raw source text of the inner expression.
    ///
    /// The lexer records the raw text; the parser re-parses it as a tinct expression
    /// and includes it as an arg in the desugared `[str ...]` call.
    Expr(String),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::OpenBracket => write!(f, "["),
            Token::CloseBracket => write!(f, "]"),
            Token::Colon => write!(f, ":"),
            Token::Semicolon => write!(f, ";"),
            Token::Dot => write!(f, "."),
            Token::Pipe => write!(f, "|"),
            Token::At => write!(f, "@"),
            Token::ImmediateAt => write!(f, "@"),
            Token::Ellipsis => write!(f, "..."),
            Token::DocSeparator => write!(f, "---"),
            Token::Newline => write!(f, "\\n"),
            Token::Comment(text) => write!(f, "# {text}"),
            Token::Int(n) => write!(f, "{n}"),
            Token::Float(n) => write!(f, "{n}"),
            Token::Identifier(s) => write!(f, "{s}"),
            Token::QuotedString(s) => write!(f, "\"{s}\""),
            Token::EscapedRef(name) => write!(f, "${name}"),
            Token::BoolLit(b) => write!(f, "{b}"),
            Token::InterpolatedString(parts) => {
                write!(f, "i\"")?;
                for part in parts {
                    match part {
                        InterpolatedPart::Literal(s) => write!(f, "{s}")?,
                        InterpolatedPart::VarRef(name) => write!(f, "${name}")?,
                        InterpolatedPart::Expr(raw) => write!(f, "${{{raw}}}")?,
                    }
                }
                write!(f, "\"")
            }
            Token::TripleQuotedString(s) => write!(f, "\"\"\"{s}\"\"\""),
            Token::TripleInterpolatedString(parts) => {
                write!(f, "i\"\"\"")?;
                for part in parts {
                    match part {
                        InterpolatedPart::Literal(s) => write!(f, "{s}")?,
                        InterpolatedPart::VarRef(name) => write!(f, "${name}")?,
                        InterpolatedPart::Expr(raw) => write!(f, "${{{raw}}}")?,
                    }
                }
                write!(f, "\"\"\"")
            }
            Token::Let => write!(f, "let"),
            Token::Case => write!(f, "case"),
        }
    }
}

/// Lexer error with message and source span.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub message: String,
    pub span: Span,
}

impl LexError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.message, self.span)
    }
}

impl std::error::Error for LexError {}

/// Maximum nesting depth for bracket expressions (enforced during tokenization).
const MAX_LEX_DEPTH: usize = 256;

/// Tokenize input string into a flat token stream.
///
/// Returns a vector of spanned tokens or an error if the input is malformed.
pub fn tokenize(input: &str) -> Result<Vec<Spanned<Token>>, LexError> {
    Lexer::new(input).tokenize()
}

struct Lexer<'a> {
    input: &'a str,
    chars: std::str::CharIndices<'a>,
    current: Option<(usize, char)>,
    line: usize,
    column: usize,
    last_newline_offset: usize,
    tokens: Vec<Spanned<Token>>,
    /// Tracks if horizontal whitespace (spaces/tabs, not newlines) was skipped before the current position.
    ///
    /// Used to disambiguate whitespace-sensitive syntax:
    /// - `word@annotation` (no gap) → ImmediateAt; `word @annotation` (gap) → At
    ///
    /// This flag is reset at the start of each token loop iteration and set by `skip_whitespace_except_newline()`.
    had_whitespace_before: bool,
    /// Bracket nesting depth (for MAX_LEX_DEPTH enforcement)
    bracket_depth: usize,
    /// True if the last token was Dot in an access chain (next identifier excludes dots)
    after_access_dot: bool,
    /// Tracks whether the last significant token was an Identifier (for ImmediateAt detection).
    ///
    /// Used to determine when `@` should emit `ImmediateAt` (immediately after a bare Identifier
    /// with no whitespace gap) vs plain `At`. This enables `x@Int` (ImmediateAt) vs `x @Int` (At).
    last_was_identifier: bool,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        let mut chars = input.char_indices();
        let current = chars.next();
        Self {
            input,
            chars,
            current,
            line: 1,
            column: 1,
            last_newline_offset: 0,
            tokens: Vec::with_capacity(input.len() / 10),
            had_whitespace_before: false,
            bracket_depth: 0,
            after_access_dot: false,
            last_was_identifier: false,
        }
    }

    fn tokenize(mut self) -> Result<Vec<Spanned<Token>>, LexError> {
        while self.current.is_some() {
            self.had_whitespace_before = false;
            self.skip_whitespace_except_newline()?;
            if self.current.is_none() {
                break;
            }
            self.next_token()?;
        }
        Ok(self.tokens)
    }

    fn current_position(&self) -> Position {
        Position {
            offset: self.current.map(|(i, _)| i).unwrap_or(self.input.len()),
            line: self.line,
            column: self.column,
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.current.map(|(_, c)| c)
    }

    fn advance(&mut self) {
        if let Some((_, c)) = self.current {
            if c == '\n' {
                self.line += 1;
                self.column = 1;
                self.last_newline_offset = self.current.map(|(i, _)| i).unwrap_or(0) + 1;
            } else if c == '\r' {
                // Only increment line if this is a bare CR (not followed by \n)
                // Peek ahead to check for CRLF
                let next = self.chars.clone().next();
                if next.map(|(_, ch)| ch) != Some('\n') {
                    self.line += 1;
                    self.column = 1;
                    self.last_newline_offset = self.current.map(|(i, _)| i).unwrap_or(0) + 1;
                } else {
                    self.column += 1;
                }
            } else {
                self.column += 1;
            }
        }
        self.current = self.chars.next();
    }

    fn skip_whitespace_except_newline(&mut self) -> Result<(), LexError> {
        while let Some(c) = self.peek_char() {
            match c {
                ' ' | '\t' => {
                    self.had_whitespace_before = true;
                    self.advance();
                }
                '\r' => {
                    // Handle CRLF. A newline resets the ImmediateAt context.
                    self.had_whitespace_before = true;
                    self.last_was_identifier = false;
                    let start = self.current_position();
                    self.advance();
                    if self.peek_char() == Some('\n') {
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Newline, Span::new(start, end)));
                    } else {
                        // Bare CR is treated as newline
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Newline, Span::new(start, end)));
                    }
                }
                '\n' => {
                    // A newline resets the ImmediateAt context.
                    self.had_whitespace_before = true;
                    self.last_was_identifier = false;
                    let start = self.current_position();
                    self.advance();
                    let end = self.current_position();
                    self.tokens
                        .push(Spanned::new(Token::Newline, Span::new(start, end)));
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn next_token(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        let c = match self.peek_char() {
            Some(c) => c,
            None => return Ok(()),
        };

        match c {
            '#' => {
                self.after_access_dot = false;
                // Comments don't update last_was_identifier (ignored for ImmediateAt detection)
                self.lex_comment()
            }
            '[' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                // Check depth before incrementing; advance first so the span
                // covers the `[` character (start..end is one char wide).
                if self.bracket_depth >= MAX_LEX_DEPTH {
                    self.advance();
                    let end = self.current_position();
                    return Err(LexError::new(
                        format!("maximum nesting depth exceeded (limit: {MAX_LEX_DEPTH})"),
                        Span::new(start, end),
                    ));
                }
                self.advance();
                self.bracket_depth += 1;
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::OpenBracket, Span::new(start, end)));
                Ok(())
            }
            ']' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                // Note: Lexer doesn't validate bracket matching (parser's job)
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::CloseBracket, Span::new(start, end)));
                Ok(())
            }
            ':' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Colon, Span::new(start, end)));
                Ok(())
            }
            ';' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Semicolon, Span::new(start, end)));
                Ok(())
            }
            '@' => {
                self.after_access_dot = false;
                self.advance();
                let end = self.current_position();

                // Emit ImmediateAt if no whitespace before and previous token was a plain Identifier.
                let token = if !self.had_whitespace_before && self.last_was_identifier {
                    Token::ImmediateAt
                } else {
                    Token::At
                };

                self.last_was_identifier = false;
                self.tokens.push(Spanned::new(token, Span::new(start, end)));
                Ok(())
            }
            '"' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                // Check for triple-quoted string `"""`
                if self.peek_ahead(1) == Some('"') && self.peek_ahead(2) == Some('"') {
                    self.lex_triple_quoted_string()
                } else {
                    self.lex_quoted_string()
                }
            }
            'i' if self.peek_ahead(1) == Some('"') => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                // Check for triple-quoted interpolated string `i"""`
                if self.peek_ahead(2) == Some('"') && self.peek_ahead(3) == Some('"') {
                    self.lex_triple_interpolated_string()
                } else {
                    self.lex_interpolated_string()
                }
            }
            '$' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.lex_var_ref()
            }
            '%' => {
                // % is a plain bare-word character. `%`, `%cwd`, `%nc` all lex as
                // Identifier tokens through the normal bare-word path — no special case.
                // is_var_ident_char does not list '%' in its denylist, so lex_bare_word_or_number
                // consumes the entire `%word` as a single Identifier token.
                self.lex_bare_word_or_number()
            }
            '|' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Pipe, Span::new(start, end)));
                Ok(())
            }
            '-' => {
                // Could be negative number or doc separator or bare word
                if self.peek_ahead(1) == Some('-') && self.peek_ahead(2) == Some('-') {
                    // Check if this is actually a doc separator (not followed by bare_word_char)
                    if !self.is_bare_word_char_at(3) {
                        self.after_access_dot = false;
                        self.lex_doc_separator()
                    } else {
                        // Bare word or number - after_access_dot handled in lex_bare_word_or_number
                        self.lex_bare_word_or_number()
                    }
                } else {
                    // Bare word or number - after_access_dot handled in lex_bare_word_or_number
                    self.lex_bare_word_or_number()
                }
            }
            '.' => {
                // Check for ellipsis, two-dots, or single dot
                self.last_was_identifier = false;
                if self.peek_ahead(1) == Some('.') {
                    if self.peek_ahead(2) == Some('.') {
                        // Ellipsis `...`
                        self.after_access_dot = false;
                        self.advance();
                        self.advance();
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Ellipsis, Span::new(start, end)));
                        Ok(())
                    } else {
                        // `..` — two consecutive dots. Emit the first Dot and let the next
                        // iteration handle the second. Range syntax has been removed.
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Dot, Span::new(start, end)));
                        self.after_access_dot = true;
                        Ok(())
                    }
                } else {
                    // '.' is always a dot-access operator. Whitespace before '.' is allowed
                    // and does not prevent dot access. This matches Nix/Jsonnet behavior.
                    self.advance();
                    let end = self.current_position();
                    self.tokens
                        .push(Spanned::new(Token::Dot, Span::new(start, end)));
                    self.after_access_dot = true;
                    Ok(())
                }
            }
            _ if c.is_ascii_digit() => {
                // Number or bare word - after_access_dot handled in lex_bare_word_or_number
                self.lex_bare_word_or_number()
            }
            _ => {
                // Bare word - after_access_dot handled in lex_bare_word_or_number
                self.lex_bare_word_or_number()
            }
        }
    }

    fn peek_ahead(&self, n: usize) -> Option<char> {
        let mut iter =
            self.input[self.current.map(|(i, _)| i).unwrap_or(self.input.len())..].chars();
        iter.nth(n)
    }

    fn is_bare_word_char_at(&self, offset: usize) -> bool {
        if let Some(c) = self.peek_ahead(offset) {
            !matches!(
                c,
                ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '$' | '|'
            )
        } else {
            false
        }
    }

    fn lex_comment(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip '#'

        let comment_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        while let Some(c) = self.peek_char() {
            if c == '\n' || c == '\r' {
                break;
            }
            self.advance();
        }
        let comment_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let text = self.input[comment_start..comment_end].to_string();

        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::Comment(text), Span::new(start, end)));
        Ok(())
    }

    fn lex_quoted_string(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip opening '"'

        let mut result = String::new();
        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    self.advance();
                    let end = self.current_position();
                    self.tokens.push(Spanned::new(
                        Token::QuotedString(result),
                        Span::new(start, end),
                    ));
                    return Ok(());
                }
                '\\' => {
                    self.advance();
                    match self.peek_char() {
                        Some('"') => {
                            result.push('"');
                            self.advance();
                        }
                        Some('\\') => {
                            result.push('\\');
                            self.advance();
                        }
                        Some('n') => {
                            result.push('\n');
                            self.advance();
                        }
                        Some('t') => {
                            result.push('\t');
                            self.advance();
                        }
                        Some('r') => {
                            result.push('\r');
                            self.advance();
                        }
                        Some(c) => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                format!("invalid escape sequence: \\{c}"),
                                Span::new(start, end),
                            ));
                        }
                        None => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated escape sequence",
                                Span::new(start, end),
                            ));
                        }
                    }
                }
                _ => {
                    result.push(c);
                    self.advance();
                }
            }
        }

        let end = self.current_position();
        Err(LexError::new("unterminated string", Span::new(start, end)))
    }

    fn lex_interpolated_string(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip 'i'
        self.advance(); // skip opening '"'

        let mut parts = Vec::new();
        let mut literal = String::new();

        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    // End of interpolated string
                    if !literal.is_empty() {
                        parts.push(InterpolatedPart::Literal(literal));
                    }
                    self.advance();
                    let end = self.current_position();
                    self.tokens.push(Spanned::new(
                        Token::InterpolatedString(parts),
                        Span::new(start, end),
                    ));
                    return Ok(());
                }
                '$' => {
                    // Check for $$ (escaped literal $)
                    if self.peek_ahead(1) == Some('$') {
                        literal.push('$');
                        self.advance();
                        self.advance();
                    } else if self.peek_ahead(1) == Some('{') {
                        // ${expr} expression interpolation — read until matching '}'
                        // Save current literal part if non-empty
                        if !literal.is_empty() {
                            parts.push(InterpolatedPart::Literal(literal.clone()));
                            literal.clear();
                        }
                        self.advance(); // skip '$'
                        self.advance(); // skip '{'

                        // Collect the inner expression, tracking brace depth.
                        // We stop at the matching '}' (depth 0 after opening brace).
                        let expr_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                        let mut depth: usize = 1;
                        let mut found_close = false;
                        while let Some(c) = self.peek_char() {
                            match c {
                                '{' => {
                                    depth += 1;
                                    self.advance();
                                }
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        found_close = true;
                                        break;
                                    }
                                    self.advance();
                                }
                                _ => {
                                    self.advance();
                                }
                            }
                        }
                        let expr_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());

                        if !found_close {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated ${...} in interpolated string",
                                Span::new(start, end),
                            ));
                        }

                        self.advance(); // skip closing '}'

                        let raw = self.input[expr_start..expr_end].trim().to_string();
                        if raw.is_empty() {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "empty ${} expression in interpolated string",
                                Span::new(start, end),
                            ));
                        }
                        parts.push(InterpolatedPart::Expr(raw));
                    } else {
                        // Variable reference $name
                        // Save current literal part if non-empty
                        if !literal.is_empty() {
                            parts.push(InterpolatedPart::Literal(literal.clone()));
                            literal.clear();
                        }
                        self.advance(); // skip '$'

                        // Collect identifier characters
                        // In interpolated strings, stop at punctuation that commonly appears in natural text
                        let ident_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                        while let Some(c) = self.peek_char() {
                            // Allow same chars as var_ident, but also stop at common punctuation (comma, etc)
                            if matches!(
                                c,
                                ' ' | '\t'
                                    | '\r'
                                    | '\n'
                                    | '['
                                    | ']'
                                    | ':'
                                    | ';'
                                    | '#'
                                    | '"'
                                    | '@'
                                    | '.'
                                    | ','
                                    | '!'
                                    | '?'
                            ) {
                                break;
                            }
                            self.advance();
                        }
                        let ident_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());

                        if ident_start == ident_end {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "bare $ without identifier in interpolated string",
                                Span::new(start, end),
                            ));
                        }

                        let name = self.input[ident_start..ident_end].to_string();
                        parts.push(InterpolatedPart::VarRef(name));
                    }
                }
                '\\' => {
                    // Handle escape sequences same as regular strings
                    self.advance();
                    match self.peek_char() {
                        Some('"') => {
                            literal.push('"');
                            self.advance();
                        }
                        Some('\\') => {
                            literal.push('\\');
                            self.advance();
                        }
                        Some('n') => {
                            literal.push('\n');
                            self.advance();
                        }
                        Some('t') => {
                            literal.push('\t');
                            self.advance();
                        }
                        Some('r') => {
                            literal.push('\r');
                            self.advance();
                        }
                        Some(c) => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                format!("invalid escape sequence: \\{c}"),
                                Span::new(start, end),
                            ));
                        }
                        None => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated escape sequence",
                                Span::new(start, end),
                            ));
                        }
                    }
                }
                _ => {
                    literal.push(c);
                    self.advance();
                }
            }
        }

        let end = self.current_position();
        Err(LexError::new(
            "unterminated interpolated string",
            Span::new(start, end),
        ))
    }

    fn lex_triple_quoted_string(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip first '"'
        self.advance(); // skip second '"'
        self.advance(); // skip third '"'

        let mut result = String::new();
        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    // Check if this is the closing `"""`
                    if self.peek_ahead(1) == Some('"') && self.peek_ahead(2) == Some('"') {
                        self.advance();
                        self.advance();
                        self.advance();
                        let end = self.current_position();
                        self.tokens.push(Spanned::new(
                            Token::TripleQuotedString(result),
                            Span::new(start, end),
                        ));
                        return Ok(());
                    } else {
                        // Single quote inside content
                        result.push(c);
                        self.advance();
                    }
                }
                '\\' => {
                    // Only process escape for `"""` inside content
                    self.advance();
                    match self.peek_char() {
                        Some('"')
                            if self.peek_ahead(1) == Some('"')
                                && self.peek_ahead(2) == Some('"') =>
                        {
                            // Escaped triple-quote: `\"""`
                            result.push('"');
                            result.push('"');
                            result.push('"');
                            self.advance();
                            self.advance();
                            self.advance();
                        }
                        Some(c) => {
                            // Other escapes: pass backslash through literally
                            result.push('\\');
                            result.push(c);
                            self.advance();
                        }
                        None => {
                            result.push('\\');
                        }
                    }
                }
                _ => {
                    result.push(c);
                    self.advance();
                }
            }
        }

        let end = self.current_position();
        Err(LexError::new(
            "unterminated triple-quoted string",
            Span::new(start, end),
        ))
    }

    fn lex_triple_interpolated_string(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip 'i'
        self.advance(); // skip first '"'
        self.advance(); // skip second '"'
        self.advance(); // skip third '"'

        let mut parts = Vec::new();
        let mut literal = String::new();

        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    // Check if this is the closing `"""`
                    if self.peek_ahead(1) == Some('"') && self.peek_ahead(2) == Some('"') {
                        if !literal.is_empty() {
                            parts.push(InterpolatedPart::Literal(literal));
                        }
                        self.advance();
                        self.advance();
                        self.advance();
                        let end = self.current_position();
                        self.tokens.push(Spanned::new(
                            Token::TripleInterpolatedString(parts),
                            Span::new(start, end),
                        ));
                        return Ok(());
                    } else {
                        // Single quote inside content
                        literal.push(c);
                        self.advance();
                    }
                }
                '$' => {
                    // Check for $$ (escaped literal $)
                    if self.peek_ahead(1) == Some('$') {
                        literal.push('$');
                        self.advance();
                        self.advance();
                    } else if self.peek_ahead(1) == Some('{') {
                        // ${expr} expression interpolation — read until matching '}'
                        // Save current literal part if non-empty
                        if !literal.is_empty() {
                            parts.push(InterpolatedPart::Literal(literal.clone()));
                            literal.clear();
                        }
                        self.advance(); // skip '$'
                        self.advance(); // skip '{'

                        // Collect the inner expression, tracking brace depth.
                        let expr_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                        let mut depth: usize = 1;
                        let mut found_close = false;
                        while let Some(c) = self.peek_char() {
                            match c {
                                '{' => {
                                    depth += 1;
                                    self.advance();
                                }
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        found_close = true;
                                        break;
                                    }
                                    self.advance();
                                }
                                _ => self.advance(),
                            }
                        }

                        let expr_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                        if !found_close {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated ${...} in triple-quoted interpolated string",
                                Span::new(start, end),
                            ));
                        }

                        self.advance(); // skip closing '}'

                        let raw = self.input[expr_start..expr_end].trim().to_string();
                        if raw.is_empty() {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "empty ${} expression in triple-quoted interpolated string",
                                Span::new(start, end),
                            ));
                        }
                        parts.push(InterpolatedPart::Expr(raw));
                    } else {
                        // Variable reference $name
                        // Save current literal part if non-empty
                        if !literal.is_empty() {
                            parts.push(InterpolatedPart::Literal(literal.clone()));
                            literal.clear();
                        }
                        self.advance(); // skip '$'

                        // Collect identifier characters
                        let ident_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                        while let Some(c) = self.peek_char() {
                            if matches!(
                                c,
                                ' ' | '\t'
                                    | '\r'
                                    | '\n'
                                    | '['
                                    | ']'
                                    | ':'
                                    | ';'
                                    | '#'
                                    | '"'
                                    | '@'
                                    | '.'
                                    | ','
                                    | '!'
                                    | '?'
                            ) {
                                break;
                            }
                            self.advance();
                        }
                        let ident_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());

                        if ident_start == ident_end {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "bare $ without identifier in triple-quoted interpolated string",
                                Span::new(start, end),
                            ));
                        }

                        let name = self.input[ident_start..ident_end].to_string();
                        parts.push(InterpolatedPart::VarRef(name));
                    }
                }
                '\\' => {
                    // Only process escape for `"""` inside content
                    self.advance();
                    match self.peek_char() {
                        Some('"')
                            if self.peek_ahead(1) == Some('"')
                                && self.peek_ahead(2) == Some('"') =>
                        {
                            // Escaped triple-quote: `\"""`
                            literal.push('"');
                            literal.push('"');
                            literal.push('"');
                            self.advance();
                            self.advance();
                            self.advance();
                        }
                        Some(c) => {
                            // Other escapes: pass backslash through literally
                            literal.push('\\');
                            literal.push(c);
                            self.advance();
                        }
                        None => {
                            literal.push('\\');
                        }
                    }
                }
                _ => {
                    literal.push(c);
                    self.advance();
                }
            }
        }

        let end = self.current_position();
        Err(LexError::new(
            "unterminated triple-quoted interpolated string",
            Span::new(start, end),
        ))
    }

    fn lex_var_ref(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance(); // skip '$'

        let ident_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        while let Some(c) = self.peek_char() {
            if self.is_var_ident_char(c) {
                self.advance();
            } else {
                break;
            }
        }
        let ident_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());

        if ident_start == ident_end {
            let end = self.current_position();
            return Err(LexError::new(
                "bare $ without identifier",
                Span::new(start, end),
            ));
        }

        let name = self.input[ident_start..ident_end].to_string();
        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::EscapedRef(name), Span::new(start, end)));
        Ok(())
    }

    fn is_var_ident_char(&self, c: char) -> bool {
        // Denylist: exclude whitespace, structural delimiters, dot (access operator), and pipe
        !matches!(
            c,
            ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '.' | '|'
        )
    }

    fn lex_doc_separator(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.last_was_identifier = false;
        self.advance();
        self.advance();
        self.advance();
        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::DocSeparator, Span::new(start, end)));
        Ok(())
    }

    fn lex_bare_word_or_number(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        let word_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let in_access_field = self.after_access_dot;
        self.after_access_dot = false; // Reset flag

        // Check for negative number
        if self.peek_char() == Some('-') {
            self.advance();
        }

        // Check for number pattern
        if let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                return self.lex_number(start, word_start, in_access_field);
            }
        }

        // Not a number, collect as bare word
        let mut char_index = 0;
        while let Some(c) = self.peek_char() {
            if in_access_field {
                // In access field context, use strict allowlist from grammar
                if self.is_access_field_char(c, char_index == 0) {
                    self.advance();
                    char_index += 1;
                } else {
                    break;
                }
            } else {
                // Identifier (new syntax): dots are NOT allowed (they are access operators).
                // Use is_var_ident_char which excludes '.', so `name.field` tokenizes
                // as Identifier("name") Dot Identifier("field") — dot access chain.
                if self.is_var_ident_char(c) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let word = self.input[word_start..word_end].to_string();
        let end = self.current_position();

        // Check for reserved keywords and boolean literals
        if word == "true" {
            self.tokens
                .push(Spanned::new(Token::BoolLit(true), Span::new(start, end)));
            self.last_was_identifier = false; // BoolLit does not trigger ImmediateAt
        } else if word == "false" {
            self.tokens
                .push(Spanned::new(Token::BoolLit(false), Span::new(start, end)));
            self.last_was_identifier = false; // BoolLit does not trigger ImmediateAt
        } else if word == "let" {
            self.tokens
                .push(Spanned::new(Token::Let, Span::new(start, end)));
            self.last_was_identifier = false; // Keywords do not trigger ImmediateAt
        } else if word == "case" {
            self.tokens
                .push(Spanned::new(Token::Case, Span::new(start, end)));
            self.last_was_identifier = false; // Keywords do not trigger ImmediateAt
        } else {
            self.tokens
                .push(Spanned::new(Token::Identifier(word), Span::new(start, end)));
            // Only plain (non-access-field) identifiers trigger ImmediateAt for annotations.
            // Access-field identifiers (after `.`) never have `@` immediately after them in
            // valid syntax — the annotation always follows the bare word in parameter position.
            self.last_was_identifier = !in_access_field;
        }
        Ok(())
    }

    fn is_access_field_char(&self, c: char, is_first: bool) -> bool {
        // Access field names (after dot in access chain) use allowlist from grammar.
        // `?` is allowed anywhere in continuation to support predicate naming (`int?`, `dict?`).
        // Grammar: access_field = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-" | "?")* }
        if is_first {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '?'
        }
    }

    fn lex_number(
        &mut self,
        start: Position,
        word_start: usize,
        in_access_field: bool,
    ) -> Result<(), LexError> {
        // Collect digits
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float (decimal point followed by digits).
        // Suppressed when in_access_field is true: in `$a.0.1`, the `.` after `0`
        // is a field-access dot, not a decimal separator. Without this guard,
        // `$a.0.1` would lex the `0.1` fragment as a Float token.
        if !in_access_field && self.peek_char() == Some('.') {
            let next = self.peek_ahead(1);
            if next.is_some() && next.unwrap().is_ascii_digit() {
                self.advance(); // skip '.'
                while let Some(c) = self.peek_char() {
                    if c.is_ascii_digit() {
                        self.advance();
                    } else {
                        break;
                    }
                }

                let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                let word = &self.input[word_start..word_end];
                let end = self.current_position();

                match word.parse::<f64>() {
                    Ok(n) => {
                        self.tokens
                            .push(Spanned::new(Token::Float(n), Span::new(start, end)));
                        self.last_was_identifier = false;
                        Ok(())
                    }
                    Err(e) => Err(LexError::new(
                        format!("invalid float literal: {e}"),
                        Span::new(start, end),
                    )),
                }
            } else {
                // Dot not followed by digit, treat as integer followed by bare word
                let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
                let word = &self.input[word_start..word_end];
                let end = self.current_position();

                match word.parse::<i64>() {
                    Ok(n) => {
                        self.tokens
                            .push(Spanned::new(Token::Int(n), Span::new(start, end)));
                        self.last_was_identifier = false;
                        Ok(())
                    }
                    Err(e) => Err(LexError::new(
                        format!("invalid integer literal: {e}"),
                        Span::new(start, end),
                    )),
                }
            }
        } else {
            // Integer
            let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
            let word = &self.input[word_start..word_end];
            let end = self.current_position();

            match word.parse::<i64>() {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::Int(n), Span::new(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid integer literal: {e}"),
                    Span::new(start, end),
                )),
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────────
// Token-level formatters — SCN serializers for tinct literals
// ────────────────────────────────────────────────────────────────────────────────
//
// These functions produce canonical tinct literal syntax for Self-Contained Normal Form
// (SCN) serialization. They are the inverse of the lexer: where tokenize() parses source
// text to Token values, these functions format values back to tinct source text.
//
// Co-location with the lexer ensures the parse↔format pair for each literal type is
// visible together, reducing drift.

/// Format an integer as a tinct literal.
pub(crate) fn fmt_int(n: i64) -> String {
    n.to_string()
}

/// Format a float as a tinct literal.
///
/// Returns an error for NaN and Inf (not representable as tinct literals).
/// Always includes a decimal point to distinguish from integers.
pub(crate) fn fmt_float(f: f64) -> Result<String, String> {
    if f.is_nan() {
        return Err("NaN cannot be serialized as a tinct literal".to_string());
    }
    if f.is_infinite() {
        return Err("Infinity cannot be serialized as a tinct literal".to_string());
    }

    let s = f.to_string();
    // Ensure a decimal point is always present to distinguish from integers.
    // Rust's f64::to_string() omits the fractional part for whole numbers (1.0 → "1"),
    // which would lex as Token::Int. Append ".0" when no point or exponent is present.
    if s.contains('.') || s.contains('e') {
        Ok(s)
    } else {
        Ok(format!("{}.0", s))
    }
}

/// Format a boolean as a tinct literal.
pub(crate) fn fmt_bool(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// Format a string as a tinct quoted literal with proper escaping.
///
/// Escapes: `\"`, `\\`, `\n`, `\r`, `\t`
/// Always uses single-line `"..."` quoting (never `"""..."""` triple-quoted strings,
/// as required by the SCN spec for stream format compatibility).
pub(crate) fn fmt_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Format a decimal as a tinct literal using the `decimal` constructor.
pub(crate) fn fmt_decimal(d: &rust_decimal::Decimal) -> String {
    format!("[decimal \"{}\"]", d)
}

/// Format a bigint as a tinct literal using the `big-int` constructor.
pub(crate) fn fmt_bigint(n: &num_bigint::BigInt) -> String {
    format!("[big-int \"{}\"]", n)
}

/// Format bytes as a tinct literal using the `bytes-of` stdlib constructor.
///
/// Returns `[bytes-of [0: b₀  1: b₁  ...]]` — an integer-keyed dict of byte values.
pub(crate) fn fmt_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("[bytes-of [");
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{i}: {b}"));
    }
    out.push_str("]]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(input: &str) -> Vec<Token> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|s| s.node)
            .collect()
    }

    fn tok_err(input: &str) -> String {
        tokenize(input).unwrap_err().message
    }

    #[test]
    fn test_basic_tokens() {
        assert_eq!(tok("[ ]"), vec![Token::OpenBracket, Token::CloseBracket]);
        assert_eq!(tok(":"), vec![Token::Colon]);
        assert_eq!(tok(";"), vec![Token::Semicolon]);
        assert_eq!(tok("@"), vec![Token::At]);
        assert_eq!(tok("..."), vec![Token::Ellipsis]);
        assert_eq!(tok("---"), vec![Token::DocSeparator]);
    }

    #[test]
    fn test_numbers() {
        assert_eq!(tok("42"), vec![Token::Int(42)]);
        assert_eq!(tok("-1"), vec![Token::Int(-1)]);
        // 3.14 tests the lexer's float parsing — intentionally not PI.
        #[allow(clippy::approx_constant)]
        {
            assert_eq!(tok("3.14"), vec![Token::Float(3.14)]);
        }
        assert_eq!(tok("-2.5"), vec![Token::Float(-2.5)]);
    }

    #[test]
    fn test_booleans() {
        assert_eq!(tok("true"), vec![Token::BoolLit(true)]);
        assert_eq!(tok("false"), vec![Token::BoolLit(false)]);
        assert_eq!(
            tok("true false"),
            vec![Token::BoolLit(true), Token::BoolLit(false)]
        );
    }

    #[test]
    fn test_bare_words() {
        assert_eq!(tok("hello"), vec![Token::Identifier("hello".into())]);
        assert_eq!(tok("my-var"), vec![Token::Identifier("my-var".into())]);
        assert_eq!(tok("has?"), vec![Token::Identifier("has?".into())]);
        // In new syntax, dots are access operators, not bare-word chars.
        // "file.txt" tokenizes as Identifier("file") Dot Identifier("txt").
        // File paths with dots must be quoted strings: "file.txt".
        assert_eq!(
            tok("file.txt"),
            vec![
                Token::Identifier("file".into()),
                Token::Dot,
                Token::Identifier("txt".into()),
            ]
        );
    }

    #[test]
    fn test_quoted_strings() {
        assert_eq!(tok(r#""hello""#), vec![Token::QuotedString("hello".into())]);
        assert_eq!(
            tok(r#""with \"quotes\"""#),
            vec![Token::QuotedString("with \"quotes\"".into())]
        );
        assert_eq!(
            tok(r#""line\nbreak""#),
            vec![Token::QuotedString("line\nbreak".into())]
        );
        assert_eq!(
            tok(r#""tab\there""#),
            vec![Token::QuotedString("tab\there".into())]
        );
    }

    #[test]
    fn test_string_errors() {
        assert!(tok_err(r#""unterminated"#).contains("unterminated string"));
        assert!(tok_err(r#""bad \x escape""#).contains("invalid escape sequence"));
    }

    #[test]
    fn test_var_refs() {
        assert_eq!(tok("$x"), vec![Token::EscapedRef("x".into())]);
        assert_eq!(tok("$my-var"), vec![Token::EscapedRef("my-var".into())]);
        assert_eq!(tok("$$"), vec![Token::EscapedRef("$".into())]);
        assert_eq!(tok("$$foo"), vec![Token::EscapedRef("$foo".into())]);
        assert_eq!(tok("$0"), vec![Token::EscapedRef("0".into())]);
    }

    #[test]
    fn test_var_ref_errors() {
        assert!(tok_err("$").contains("bare $"));
    }

    #[test]
    fn test_access_chains() {
        assert_eq!(
            tok("$a.b"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("b".into())
            ]
        );
        assert_eq!(
            tok("$a.b.c"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("b".into()),
                Token::Dot,
                Token::Identifier("c".into())
            ]
        );
        // Access field names only allow [a-zA-Z0-9_-?] per grammar
        assert_eq!(
            tok("$a.foo!bar"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("foo".into()),
                Token::Identifier("!bar".into())
            ]
        );
    }

    #[test]
    fn test_whitespace_sensitivity() {
        // $a.b is dot access
        assert_eq!(
            tok("$a.b"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("b".into())
            ]
        );

        // $a .b — dot always emits Dot token; whitespace before dot is allowed (same behavior as Nix/Jsonnet).
        assert_eq!(
            tok("$a .b"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("b".into())
            ]
        );

        // a.b — dots are access operators; tokenizes as Identifier Dot Identifier
        assert_eq!(
            tok("a.b"),
            vec![
                Token::Identifier("a".into()),
                Token::Dot,
                Token::Identifier("b".into())
            ]
        );
    }

    #[test]
    fn test_bracket_is_always_open_bracket() {
        // BracketAccess has been removed. `[` always emits OpenBracket regardless of
        // what preceded it. Bracket access syntax ($a[0]) no longer exists in the grammar.
        assert_eq!(
            tok("$a[0]"),
            vec![
                Token::EscapedRef("a".into()),
                Token::OpenBracket,
                Token::Int(0),
                Token::CloseBracket
            ]
        );
        assert_eq!(
            tok("$a [0]"),
            vec![
                Token::EscapedRef("a".into()),
                Token::OpenBracket,
                Token::Int(0),
                Token::CloseBracket
            ]
        );
        assert_eq!(tok("]["), vec![Token::CloseBracket, Token::OpenBracket]);
        assert_eq!(
            tok("[call $f][0]"),
            vec![
                Token::OpenBracket,
                Token::Identifier("call".into()),
                Token::EscapedRef("f".into()),
                Token::CloseBracket,
                Token::OpenBracket,
                Token::Int(0),
                Token::CloseBracket
            ]
        );
        assert_eq!(
            tok("call [x]"),
            vec![
                Token::Identifier("call".into()),
                Token::OpenBracket,
                Token::Identifier("x".into()),
                Token::CloseBracket
            ]
        );
    }

    #[test]
    fn test_double_dot_is_two_dots() {
        // `..` always emits two Dot tokens — range syntax has been removed.
        assert_eq!(
            tok("file..bak"),
            vec![
                Token::Identifier("file".into()),
                Token::Dot,
                Token::Dot,
                Token::Identifier("bak".into())
            ]
        );
        assert_eq!(
            tok("$a..b"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Dot,
                Token::Identifier("b".into())
            ]
        );
    }

    #[test]
    fn test_comments() {
        assert_eq!(tok("# comment"), vec![Token::Comment(" comment".into())]);
        assert_eq!(
            tok("# comment\n42"),
            vec![
                Token::Comment(" comment".into()),
                Token::Newline,
                Token::Int(42)
            ]
        );
    }

    #[test]
    fn test_newlines() {
        assert_eq!(tok("\n"), vec![Token::Newline]);
        assert_eq!(tok("\n\n"), vec![Token::Newline, Token::Newline]);
        assert_eq!(
            tok("a\nb"),
            vec![
                Token::Identifier("a".into()),
                Token::Newline,
                Token::Identifier("b".into())
            ]
        );
    }

    #[test]
    fn test_bracket_always_open() {
        // `[` always emits OpenBracket — bracket access syntax has been removed.
        assert_eq!(
            tok("]\n["),
            vec![Token::CloseBracket, Token::Newline, Token::OpenBracket]
        );
        assert_eq!(
            tok("]\r\n["),
            vec![Token::CloseBracket, Token::Newline, Token::OpenBracket]
        );
        // Same-line `][` is also OpenBracket now (no BracketAccess)
        assert_eq!(tok("]["), vec![Token::CloseBracket, Token::OpenBracket]);
    }

    #[test]
    fn test_crlf() {
        assert_eq!(tok("\r\n"), vec![Token::Newline]);
        assert_eq!(
            tok("a\r\nb"),
            vec![
                Token::Identifier("a".into()),
                Token::Newline,
                Token::Identifier("b".into())
            ]
        );
    }

    #[test]
    fn test_doc_separator() {
        assert_eq!(tok("---"), vec![Token::DocSeparator]);
        assert_eq!(tok("--- "), vec![Token::DocSeparator]);
        assert_eq!(tok("----"), vec![Token::Identifier("----".into())]);
    }

    #[test]
    fn test_percent_bare_words() {
        // % is a plain bare-word character — no special-case path.
        // %defaults, %, %+ all lex as Identifier through the normal bare-word scanner.
        assert_eq!(
            tok("%defaults"),
            vec![Token::Identifier("%defaults".into())]
        );
        assert_eq!(tok("%"), vec![Token::Identifier("%".into())]);
        assert_eq!(tok("%+"), vec![Token::Identifier("%+".into())]);
        assert_eq!(tok("%config"), vec![Token::Identifier("%config".into())]);
        assert_eq!(
            tok("%raw_data"),
            vec![Token::Identifier("%raw_data".into())]
        );
        // Injected cap names
        assert_eq!(tok("%cwd"), vec![Token::Identifier("%cwd".into())]);
        assert_eq!(tok("%libdir"), vec![Token::Identifier("%libdir".into())]);
        assert_eq!(tok("%stdin"), vec![Token::Identifier("%stdin".into())]);
        assert_eq!(tok("%nc"), vec![Token::Identifier("%nc".into())]);
    }

    #[test]
    fn test_percent_word_dot_access() {
        // is_var_ident_char excludes '.', so %base.x tokenizes as three tokens.
        // %base.x  →  Identifier("%base"), Dot, Identifier("x")
        assert_eq!(
            tok("%base.x"),
            vec![
                Token::Identifier("%base".into()),
                Token::Dot,
                Token::Identifier("x".into()),
            ]
        );
        // Chained access: %cfg.server.port
        assert_eq!(
            tok("%cfg.server.port"),
            vec![
                Token::Identifier("%cfg".into()),
                Token::Dot,
                Token::Identifier("server".into()),
                Token::Dot,
                Token::Identifier("port".into()),
            ]
        );
        // %cwd.field — injected cap dot access
        assert_eq!(
            tok("%cwd.field"),
            vec![
                Token::Identifier("%cwd".into()),
                Token::Dot,
                Token::Identifier("field".into()),
            ]
        );
        // Whitespace before dot: permitted. '.' always emits a Dot token regardless of preceding whitespace.
        assert_eq!(
            tok("%base .x"),
            vec![
                Token::Identifier("%base".into()),
                Token::Dot,
                Token::Identifier("x".into()),
            ]
        );
    }

    #[test]
    fn test_complex_example() {
        let input = "[call $f x: 42]";
        assert_eq!(
            tok(input),
            vec![
                Token::OpenBracket,
                Token::Identifier("call".into()),
                Token::EscapedRef("f".into()),
                Token::Identifier("x".into()),
                Token::Colon,
                Token::Int(42),
                Token::CloseBracket
            ]
        );
    }

    #[test]
    fn test_ellipsis_vs_double_dot() {
        // `...` is Ellipsis (unchanged). `..` is always two Dot tokens (range removed).
        assert_eq!(tok("..."), vec![Token::Ellipsis]);
        // `$a[..]` — `$a` EscapedRef, `[` OpenBracket, `..` two Dots, `]` CloseBracket
        assert_eq!(
            tok("$a[..]"),
            vec![
                Token::EscapedRef("a".into()),
                Token::OpenBracket,
                Token::Dot,
                Token::Dot,
                Token::CloseBracket
            ]
        );
    }

    #[test]
    fn test_position_tracking() {
        let result = tokenize("a\nb").unwrap();
        assert_eq!(result[0].span.start.line, 1);
        assert_eq!(result[0].span.start.column, 1);
        assert_eq!(result[1].span.start.line, 1);
        assert_eq!(result[1].span.end.line, 2);
        assert_eq!(result[2].span.start.line, 2);
        assert_eq!(result[2].span.start.column, 1);
    }

    #[test]
    fn test_bare_cr_line_tracking() {
        // Test bare CR (Mac Classic line ending) increments line counter
        let result = tokenize("a\rb").unwrap();
        assert_eq!(result.len(), 3); // Identifier, Newline, Identifier
        assert_eq!(result[0].span.start.line, 1);
        assert_eq!(result[0].span.start.column, 1);
        assert_eq!(result[1].span.start.line, 1); // Newline starts on line 1
        assert_eq!(result[1].span.end.line, 2); // Newline ends on line 2
        assert_eq!(result[2].span.start.line, 2);
        assert_eq!(result[2].span.start.column, 1);
    }

    #[test]
    fn test_integer_overflow() {
        // i64::MAX + 1 should produce an error
        let err = tok_err("9223372036854775808");
        assert!(err.contains("invalid integer literal"));
    }

    #[test]
    fn test_float_overflow() {
        // Scientific notation is not supported by the lexer
        // 1e309 tokenizes as integer 1 followed by bare word "e309"
        // This documents current behavior - scientific notation may be added later
        let tokens = tok("1e309");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], Token::Int(1));
        assert_eq!(tokens[1], Token::Identifier("e309".into()));
    }

    #[test]
    fn test_access_field_first_char() {
        // Access field must start with ASCII_ALPHA or '_' per grammar
        // $a.1 should tokenize as EscapedRef, Dot, Int (not an access chain)
        assert_eq!(
            tok("$a.1"),
            vec![Token::EscapedRef("a".into()), Token::Dot, Token::Int(1)]
        );

        // $a._priv should work (underscore is allowed as first char)
        assert_eq!(
            tok("$a._priv"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("_priv".into())
            ]
        );

        // $a.-foo should not work (hyphen not allowed as first char)
        // After dot, next char is '-', which is not alphabetic or '_',
        // so access field ends immediately and '-foo' is separate
        assert_eq!(
            tok("$a.-foo"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("-foo".into())
            ]
        );
    }

    #[test]
    fn test_empty_input() {
        // Empty input should return empty token list
        assert_eq!(tok(""), Vec::<Token>::new());
    }

    #[test]
    fn test_immediate_at() {
        // Identifier followed by @ with no whitespace — ImmediateAt
        assert_eq!(
            tok("x@Int"),
            vec![
                Token::Identifier("x".into()),
                Token::ImmediateAt,
                Token::Identifier("Int".into())
            ]
        );

        // Identifier followed by @ with whitespace — At
        assert_eq!(
            tok("x @Int"),
            vec![
                Token::Identifier("x".into()),
                Token::At,
                Token::Identifier("Int".into())
            ]
        );

        // EscapedRef followed by @ — At (not ImmediateAt, only fires after Identifier)
        assert_eq!(
            tok("$var@Int"),
            vec![
                Token::EscapedRef("var".into()),
                Token::At,
                Token::Identifier("Int".into())
            ]
        );
    }

    #[test]
    fn test_lex_depth_limit() {
        // 257 opening brackets exceeds MAX_LEX_DEPTH (256) — lexer must return an error.
        let input = "[".repeat(257);
        let result = tokenize(&input);
        assert!(result.is_err(), "expected error for 257 nested brackets");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("maximum nesting depth exceeded"),
            "expected depth error message, got: {}",
            err.message
        );
        // Span must be one character wide (covers the offending `[`, not zero-width).
        assert_ne!(
            err.span.start, err.span.end,
            "error span must not be zero-width"
        );
    }

    #[test]
    fn test_interpolated_string_expr_basic() {
        // ${expr} produces InterpolatedPart::Expr with the inner raw text
        let tokens = tok(r#"i"result: ${[+ $x 1]}""#);
        assert_eq!(tokens.len(), 1);
        match &tokens[0] {
            Token::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0], InterpolatedPart::Literal("result: ".into()));
                assert_eq!(parts[1], InterpolatedPart::Expr("[+ $x 1]".into()));
            }
            other => panic!("expected InterpolatedString, got {:?}", other),
        }
    }

    #[test]
    fn test_interpolated_string_expr_nested_braces() {
        // Nested braces inside ${...} are tracked correctly
        let tokens = tok(r#"i"${nested {brace}}""#);
        assert_eq!(tokens.len(), 1);
        match &tokens[0] {
            Token::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0], InterpolatedPart::Expr("nested {brace}".into()));
            }
            other => panic!("expected InterpolatedString, got {:?}", other),
        }
    }

    #[test]
    fn test_interpolated_string_expr_with_varref() {
        // Mix of $name and ${expr} parts
        let tokens = tok(r#"i"$name is ${[+ $x 1]}""#);
        assert_eq!(tokens.len(), 1);
        match &tokens[0] {
            Token::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0], InterpolatedPart::VarRef("name".into()));
                assert_eq!(parts[1], InterpolatedPart::Literal(" is ".into()));
                assert_eq!(parts[2], InterpolatedPart::Expr("[+ $x 1]".into()));
            }
            other => panic!("expected InterpolatedString, got {:?}", other),
        }
    }

    #[test]
    fn test_interpolated_string_expr_unterminated() {
        // Unterminated ${...} is a lex error
        let err = tok_err(r#"i"${unclosed""#);
        assert!(
            err.contains("unterminated ${...}"),
            "expected unterminated error, got: {}",
            err
        );
    }

    #[test]
    fn test_interpolated_string_expr_empty() {
        // Empty ${} is a lex error
        let err = tok_err(r#"i"${}""#);
        assert!(
            err.contains("empty ${}"),
            "expected empty expr error, got: {}",
            err
        );
    }

    #[test]
    fn test_interpolated_string_dollar_dollar_unchanged() {
        // $$ consumes two chars and emits literal "$"; the following {foo} is plain text
        // because `{` is not a special char in interpolated strings (only `${` is special).
        // So i"$${foo}" → Literal("${foo}") — one part.
        let tokens = tok(r#"i"$${foo}""#);
        assert_eq!(tokens.len(), 1);
        match &tokens[0] {
            Token::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 1, "expected exactly one part");
                assert_eq!(parts[0], InterpolatedPart::Literal("${foo}".into()));
            }
            other => panic!("expected InterpolatedString, got {:?}", other),
        }
    }
}
