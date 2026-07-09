//! Hand-written tokenizer for tinct.
//!
//! Produces a flat token stream with accurate source spans. Used by the formatter
//! and eventually by an iterative parser.
//!
//! See doc/02-syntax.md §Tokenization Rules for the full specification.

use std::fmt;
use std::sync::Arc;

use crate::ast::{Position, SourceFile, Span, Spanned};

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
    /// Unsigned 64-bit integer literal (`42u`, `0xFFu`)
    U64Lit(u64),
    /// Float literal
    Float(f64),
    /// Identifier (bare word — variable reference in value position)
    Identifier(String),
    /// Quoted string literal (escapes already processed)
    QuotedString(String),
    /// Escaped reference `$name` (disambiguator in head/key positions)
    EscapedRef(String),
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
            Token::U64Lit(n) => write!(f, "{n}u"),
            Token::Float(n) => write!(f, "{n}"),
            Token::Identifier(s) => write!(f, "{s}"),
            Token::QuotedString(s) => write!(f, "\"{s}\""),
            Token::EscapedRef(name) => write!(f, "${name}"),
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
    let sf = Arc::new(SourceFile {
        path: Arc::from("<tokenize>"),
        content: Arc::from(input),
    });
    Lexer::new(input, sf).tokenize()
}

/// Tokenize input string with an explicit source file for accurate span attribution.
pub fn tokenize_with_file(
    input: &str,
    source_file: Arc<SourceFile>,
) -> Result<Vec<Spanned<Token>>, LexError> {
    Lexer::new(input, source_file).tokenize()
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
    /// Source file shared across all spans produced by this lexer.
    source_file: Arc<SourceFile>,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str, source_file: Arc<SourceFile>) -> Self {
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
            source_file,
        }
    }

    fn make_span(&self, start: Position, end: Position) -> Span {
        Span::new(start, end, Arc::clone(&self.source_file))
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
                            .push(Spanned::new(Token::Newline, self.make_span(start, end)));
                    } else {
                        // Bare CR is treated as newline
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Newline, self.make_span(start, end)));
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
                        .push(Spanned::new(Token::Newline, self.make_span(start, end)));
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
                        self.make_span(start, end),
                    ));
                }
                self.advance();
                self.bracket_depth += 1;
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::OpenBracket, self.make_span(start, end)));
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
                    .push(Spanned::new(Token::CloseBracket, self.make_span(start, end)));
                Ok(())
            }
            ':' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Colon, self.make_span(start, end)));
                Ok(())
            }
            ';' => {
                self.after_access_dot = false;
                self.last_was_identifier = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Semicolon, self.make_span(start, end)));
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
                self.tokens.push(Spanned::new(token, self.make_span(start, end)));
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
                    .push(Spanned::new(Token::Pipe, self.make_span(start, end)));
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
                            .push(Spanned::new(Token::Ellipsis, self.make_span(start, end)));
                        Ok(())
                    } else {
                        // `..` — two consecutive dots. Emit the first Dot and let the next
                        // iteration handle the second.
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Dot, self.make_span(start, end)));
                        self.after_access_dot = true;
                        Ok(())
                    }
                } else {
                    // '.' is always a dot-access operator. Whitespace before '.' is allowed
                    // and does not prevent dot access. This matches Nix/Jsonnet behavior.
                    self.advance();
                    let end = self.current_position();
                    self.tokens
                        .push(Spanned::new(Token::Dot, self.make_span(start, end)));
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
            .push(Spanned::new(Token::Comment(text), self.make_span(start, end)));
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
                        self.make_span(start, end),
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
                                self.make_span(start, end),
                            ));
                        }
                        None => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated escape sequence",
                                self.make_span(start, end),
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
        Err(LexError::new("unterminated string", self.make_span(start, end)))
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
                        self.make_span(start, end),
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
                                self.make_span(start, end),
                            ));
                        }

                        self.advance(); // skip closing '}'

                        let raw = self.input[expr_start..expr_end].trim().to_string();
                        if raw.is_empty() {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "empty ${} expression in interpolated string",
                                self.make_span(start, end),
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
                                self.make_span(start, end),
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
                                self.make_span(start, end),
                            ));
                        }
                        None => {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "unterminated escape sequence",
                                self.make_span(start, end),
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
            self.make_span(start, end),
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
                            self.make_span(start, end),
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
            self.make_span(start, end),
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
                            self.make_span(start, end),
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
                                self.make_span(start, end),
                            ));
                        }

                        self.advance(); // skip closing '}'

                        let raw = self.input[expr_start..expr_end].trim().to_string();
                        if raw.is_empty() {
                            let end = self.current_position();
                            return Err(LexError::new(
                                "empty ${} expression in triple-quoted interpolated string",
                                self.make_span(start, end),
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
                                self.make_span(start, end),
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
            self.make_span(start, end),
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
                self.make_span(start, end),
            ));
        }

        let name = self.input[ident_start..ident_end].to_string();
        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::EscapedRef(name), self.make_span(start, end)));
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
            .push(Spanned::new(Token::DocSeparator, self.make_span(start, end)));
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

        // Check for reserved keywords
        if word == "let" {
            self.tokens
                .push(Spanned::new(Token::Let, self.make_span(start, end)));
            self.last_was_identifier = false; // Keywords do not trigger ImmediateAt
        } else if word == "case" {
            self.tokens
                .push(Spanned::new(Token::Case, self.make_span(start, end)));
            self.last_was_identifier = false; // Keywords do not trigger ImmediateAt
        } else {
            self.tokens
                .push(Spanned::new(Token::Identifier(word), self.make_span(start, end)));
            // Only plain (non-access-field) identifiers trigger ImmediateAt for annotations.
            // Access-field identifiers (after `.`) never have `@` immediately after them in
            // valid syntax — the annotation always follows the bare word in parameter position.
            self.last_was_identifier = !in_access_field;
        }
        Ok(())
    }

    fn is_access_field_char(&self, c: char, _is_first: bool) -> bool {
        // Access field names use the same expansive identifier rules as general identifiers.
        // Tinct identifiers allow nearly any character — the denylist is very short.
        // Using a restrictive allowlist here would reject valid identifiers like `%`, `!`, etc.
        self.is_var_ident_char(c)
    }

    fn lex_number(
        &mut self,
        start: Position,
        word_start: usize,
        in_access_field: bool,
    ) -> Result<(), LexError> {
        // Determine sign: word_start points to '-' if negative, or to first digit.
        // lex_bare_word_or_number advanced past '-' (if present), then verified peek_char()
        // is a digit and called us — the digit is still current (not yet consumed).
        let is_negative = self.input.as_bytes().get(word_start) == Some(&b'-');

        // Peek at first digit to check for radix prefix or leading-zero octal.
        let first_digit = self.peek_char().unwrap(); // caller guarantees a digit is here

        if first_digit == '0' && !is_negative {
            // Peek ahead one char to see if this is a radix prefix
            match self.peek_ahead(1) {
                Some('x') | Some('X') => {
                    return self.lex_radix_number(start, 16, "0x", in_access_field);
                }
                Some('b') | Some('B') => {
                    return self.lex_radix_number(start, 2, "0b", in_access_field);
                }
                Some('o') | Some('O') => {
                    return self.lex_radix_number(start, 8, "0o", in_access_field);
                }
                Some(c) if c.is_ascii_digit() => {
                    // Leading-zero octal: 0755, 0644, etc.
                    return self.lex_octal_leading_zero(start, in_access_field);
                }
                _ => {
                    // Plain '0' — fall through to normal decimal handling
                }
            }
        }

        // ── Decimal integer or float (possibly with underscore separators) ─────────
        // Advance past the first digit (already verified by caller to be ascii_digit).
        self.advance();

        // Collect remaining decimal digits, allowing underscore separators.
        loop {
            match self.peek_char() {
                Some('_') => {
                    // Underscore separator — peek ahead to ensure it is followed by a digit.
                    match self.peek_ahead(1) {
                        Some(c) if c.is_ascii_digit() => {
                            self.advance(); // consume '_'
                        }
                        _ => {
                            // Trailing underscore or double-underscore: error.
                            self.advance(); // consume the bad '_'
                            let end = self.current_position();
                            return Err(LexError::new(
                                "invalid number literal: underscore must be followed by a digit",
                                self.make_span(start, end),
                            ));
                        }
                    }
                }
                Some(c) if c.is_ascii_digit() => {
                    self.advance();
                }
                _ => break,
            }
        }

        // Check for float: decimal point followed by a digit (suppressed in access fields).
        let has_dot = !in_access_field
            && self.peek_char() == Some('.')
            && self
                .peek_ahead(1)
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);

        if has_dot {
            self.advance(); // consume '.'
                            // Collect fractional digits (underscore separators allowed)
            loop {
                match self.peek_char() {
                    Some('_') => {
                        match self.peek_ahead(1) {
                            Some(c) if c.is_ascii_digit() => {
                                self.advance(); // consume '_'
                            }
                            _ => {
                                self.advance();
                                let end = self.current_position();
                                return Err(LexError::new(
                                    "invalid number literal: underscore must be followed by a digit",
                                    self.make_span(start, end),
                                ));
                            }
                        }
                    }
                    Some(c) if c.is_ascii_digit() => {
                        self.advance();
                    }
                    _ => break,
                }
            }

            // Optional exponent: 'e'/'E' followed by optional sign and digits.
            self.maybe_consume_exponent(start)?;

            return self.emit_float(start, word_start);
        }

        // No decimal point — check for scientific notation (e.g., 1e6, 1e-3) without a dot.
        // 'e'/'E' here always produces a Float (not Int).
        if matches!(self.peek_char(), Some('e') | Some('E')) {
            // Peek ahead to confirm valid exponent form: optional sign then digit.
            let after_e = match self.peek_ahead(1) {
                Some('+') | Some('-') => self.peek_ahead(2),
                other => other,
            };
            if after_e.map(|c| c.is_ascii_digit()).unwrap_or(false) {
                self.maybe_consume_exponent(start)?;
                return self.emit_float(start, word_start);
            }
            // Not a valid exponent — treat 'e...' as end of number (identifier follows).
        }

        // Integer. Check for trailing suffix (disallow trailing identifier chars).
        self.emit_integer(start, word_start, is_negative)
    }

    /// Consume an exponent suffix `e[+-]?digits` at the current position.
    /// Called when `peek_char()` is 'e' or 'E'.
    fn maybe_consume_exponent(&mut self, start: Position) -> Result<(), LexError> {
        match self.peek_char() {
            Some('e') | Some('E') => {
                self.advance(); // consume 'e'/'E'
                                // Optional sign
                if matches!(self.peek_char(), Some('+') | Some('-')) {
                    self.advance();
                }
                // Must have at least one digit
                if !matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                    let end = self.current_position();
                    return Err(LexError::new(
                        "invalid number literal: expected digit after exponent",
                        self.make_span(start, end),
                    ));
                }
                while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                    self.advance();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Emit a `Token::Float` by stripping underscore separators from `self.input[word_start..current]`
    /// and parsing with `str::parse::<f64>`.
    fn emit_float(&mut self, start: Position, word_start: usize) -> Result<(), LexError> {
        // Reject trailing `u` suffix on floats (not meaningful).
        if self.peek_char() == Some('u') {
            self.advance();
            let end = self.current_position();
            return Err(LexError::new(
                "invalid number literal: `u` suffix is not valid on float literals",
                self.make_span(start, end),
            ));
        }

        // Guard: no trailing identifier characters (e.g., 1.5abc).
        self.check_no_trailing_ident_chars(start)?;

        let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let raw = &self.input[word_start..word_end];
        // Strip underscore separators for parsing.
        let clean: String = raw.chars().filter(|&c| c != '_').collect();
        let end = self.current_position();

        match clean.parse::<f64>() {
            Ok(n) => {
                self.tokens
                    .push(Spanned::new(Token::Float(n), self.make_span(start, end)));
                self.last_was_identifier = false;
                Ok(())
            }
            Err(e) => Err(LexError::new(
                format!("invalid float literal: {e}"),
                self.make_span(start, end),
            )),
        }
    }

    /// Emit a `Token::Int` or `Token::U64Lit` for a decimal integer starting at `word_start`.
    fn emit_integer(
        &mut self,
        start: Position,
        word_start: usize,
        is_negative: bool,
    ) -> Result<(), LexError> {
        // Check for `u` suffix → Token::U64Lit.
        let is_u64 = self.peek_char() == Some('u');
        if is_u64 {
            if is_negative {
                self.advance();
                let end = self.current_position();
                return Err(LexError::new(
                    "invalid number literal: `u` suffix cannot be used with negative numbers",
                    self.make_span(start, end),
                ));
            }
            self.advance(); // consume 'u'
        }

        // Guard: no trailing identifier characters (e.g., 42abc).
        self.check_no_trailing_ident_chars(start)?;

        let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        // Slice excludes the trailing 'u' because we already advanced past it.
        let raw = &self.input[word_start..word_end];
        // Strip 'u' suffix from the raw slice if still present (shouldn't be, but guard).
        let without_suffix = raw.trim_end_matches('u');
        // Strip underscore separators.
        let clean: String = without_suffix.chars().filter(|&c| c != '_').collect();
        let end = self.current_position();

        if is_u64 {
            match clean.parse::<u64>() {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::U64Lit(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid u64 literal: {e}"),
                    self.make_span(start, end),
                )),
            }
        } else {
            match clean.parse::<i64>() {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::Int(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid integer literal: {e}"),
                    self.make_span(start, end),
                )),
            }
        }
    }

    /// Guard: the character immediately after the number must not be a bare-word identifier
    /// character (to prevent `42abc` from silently lexing as `42` + `abc`).
    fn check_no_trailing_ident_chars(&self, start: Position) -> Result<(), LexError> {
        if let Some(c) = self.peek_char() {
            // Characters that are valid bare-word chars but not valid number suffixes
            // (excluding 'u' which is handled by the caller, 'e'/'E' handled by exponent logic,
            // and '.' handled by float logic).
            if c.is_ascii_alphabetic() && c != 'u' && c != 'e' && c != 'E' {
                // Peek ahead: if this alphabetic char is part of a longer identifier
                // (the character itself plus what follows is an identifier-like sequence),
                // it's an error. We already know 'u'/'e'/'E' are handled, so any other
                // ASCII alphabetic here is a trailing-identifier error.
                let end = self.current_position();
                return Err(LexError::new(
                    format!("invalid number literal: unexpected character `{c}` after number"),
                    self.make_span(start, end),
                ));
            }
        }
        Ok(())
    }

    /// Lex a radix-prefixed integer: `0x...` (hex), `0b...` (binary), `0o...` (octal).
    /// On entry, current is pointing at `0`; `prefix` is "0x", "0b", or "0o".
    fn lex_radix_number(
        &mut self,
        start: Position,
        radix: u32,
        prefix: &str,
        _in_access_field: bool,
    ) -> Result<(), LexError> {
        self.advance(); // consume '0'
        self.advance(); // consume 'x'/'b'/'o' (or uppercase variant)

        let digit_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());

        // Collect digits appropriate for this radix, with underscore separators.
        let mut digit_count = 0usize;
        loop {
            match self.peek_char() {
                Some('_') => {
                    match self.peek_ahead(1) {
                        Some(c) if self.is_radix_digit(c, radix) => {
                            self.advance(); // consume '_'
                        }
                        _ => {
                            self.advance();
                            let end = self.current_position();
                            return Err(LexError::new(
                                "invalid number literal: underscore must be followed by a digit",
                                self.make_span(start, end),
                            ));
                        }
                    }
                }
                Some(c) if self.is_radix_digit(c, radix) => {
                    self.advance();
                    digit_count += 1;
                }
                Some(c) if c.is_ascii_alphanumeric() && c != 'u' => {
                    // Invalid digit for this radix (e.g., '2' in binary, '9' in octal, 'g' in hex)
                    self.advance();
                    let end = self.current_position();
                    return Err(LexError::new(
                        format!(
                            "invalid digit `{c}` in {} literal",
                            match radix {
                                2 => "binary",
                                8 => "octal",
                                16 => "hexadecimal",
                                _ => "radix",
                            }
                        ),
                        self.make_span(start, end),
                    ));
                }
                _ => break,
            }
        }

        if digit_count == 0 {
            let end = self.current_position();
            return Err(LexError::new(
                format!("invalid number literal: expected digits after `{prefix}`"),
                self.make_span(start, end),
            ));
        }

        // Check for `u` suffix.
        let is_u64 = self.peek_char() == Some('u');
        if is_u64 {
            self.advance(); // consume 'u'
        }

        // Guard: no trailing identifier characters.
        self.check_no_trailing_ident_chars(start)?;

        let digit_end = if is_u64 {
            // digit_end excludes 'u'; current has already advanced past 'u'.
            self.current.map(|(i, _)| i).unwrap_or(self.input.len()) - 1
        } else {
            self.current.map(|(i, _)| i).unwrap_or(self.input.len())
        };

        // Reconstruct digits (no prefix, no suffix, no underscores).
        let raw = &self.input[digit_start..digit_end];
        let clean: String = raw.chars().filter(|&c| c != '_').collect();
        let end = self.current_position();

        if is_u64 {
            match u64::from_str_radix(&clean, radix) {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::U64Lit(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid u64 literal: {e}"),
                    self.make_span(start, end),
                )),
            }
        } else {
            match i64::from_str_radix(&clean, radix) {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::Int(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid integer literal: {e}"),
                    self.make_span(start, end),
                )),
            }
        }
    }

    /// Returns true if `c` is a valid digit for `radix`.
    fn is_radix_digit(&self, c: char, radix: u32) -> bool {
        match radix {
            2 => matches!(c, '0' | '1'),
            8 => matches!(c, '0'..='7'),
            10 => c.is_ascii_digit(),
            16 => c.is_ascii_hexdigit(),
            _ => false,
        }
    }

    /// Lex a leading-zero octal literal: `0755`, `0644`, etc.
    /// On entry, current is on `0` and peek_ahead(1) is a decimal digit.
    fn lex_octal_leading_zero(
        &mut self,
        start: Position,
        _in_access_field: bool,
    ) -> Result<(), LexError> {
        self.advance(); // consume '0'

        let digit_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let mut digit_count = 0usize;

        loop {
            match self.peek_char() {
                Some('0'..='7') => {
                    self.advance();
                    digit_count += 1;
                }
                Some(c) if matches!(c, '8' | '9') => {
                    // Invalid octal digit
                    self.advance();
                    let end = self.current_position();
                    return Err(LexError::new(
                        format!("invalid digit `{c}` in octal literal"),
                        self.make_span(start, end),
                    ));
                }
                _ => break,
            }
        }

        if digit_count == 0 {
            // Just '0' with no following octal digits — plain zero.
            let end = self.current_position();
            self.tokens
                .push(Spanned::new(Token::Int(0), self.make_span(start, end)));
            self.last_was_identifier = false;
            return Ok(());
        }

        // Check for `u` suffix.
        let is_u64 = self.peek_char() == Some('u');
        if is_u64 {
            self.advance();
        }

        // Guard: no trailing identifier characters.
        self.check_no_trailing_ident_chars(start)?;

        let digit_end = if is_u64 {
            self.current.map(|(i, _)| i).unwrap_or(self.input.len()) - 1
        } else {
            self.current.map(|(i, _)| i).unwrap_or(self.input.len())
        };

        let raw = &self.input[digit_start..digit_end];
        let end = self.current_position();

        if is_u64 {
            match u64::from_str_radix(raw, 8) {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::U64Lit(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid u64 literal: {e}"),
                    self.make_span(start, end),
                )),
            }
        } else {
            match i64::from_str_radix(raw, 8) {
                Ok(n) => {
                    self.tokens
                        .push(Spanned::new(Token::Int(n), self.make_span(start, end)));
                    self.last_was_identifier = false;
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid integer literal: {e}"),
                    self.make_span(start, end),
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

#[allow(dead_code)] // Used by Display impls for U64 literals
/// Format an unsigned 64-bit integer as a tinct literal (with `u` suffix).
pub(crate) fn fmt_u64(n: u64) -> String {
    format!("{n}u")
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
    fn test_hex_literals() {
        assert_eq!(tok("0xFF"), vec![Token::Int(255)]);
        assert_eq!(tok("0xff"), vec![Token::Int(255)]);
        assert_eq!(tok("0XFF"), vec![Token::Int(255)]);
        assert_eq!(tok("0x0"), vec![Token::Int(0)]);
        assert_eq!(tok("0x1A2B"), vec![Token::Int(0x1A2B)]);
        assert_eq!(tok("0xFFu"), vec![Token::U64Lit(255)]);
        assert_eq!(tok("0x1_0"), vec![Token::Int(0x10)]);
    }

    #[test]
    fn test_binary_literals() {
        assert_eq!(tok("0b1010"), vec![Token::Int(0b1010)]);
        assert_eq!(tok("0b0"), vec![Token::Int(0)]);
        assert_eq!(tok("0B1"), vec![Token::Int(1)]);
        assert_eq!(tok("0b1010u"), vec![Token::U64Lit(0b1010)]);
        assert_eq!(tok("0b1_0"), vec![Token::Int(0b10)]);
    }

    #[test]
    fn test_octal_prefix_literals() {
        assert_eq!(tok("0o77"), vec![Token::Int(0o77)]);
        assert_eq!(tok("0O77"), vec![Token::Int(0o77)]);
        assert_eq!(tok("0o0"), vec![Token::Int(0)]);
        assert_eq!(tok("0o77u"), vec![Token::U64Lit(0o77)]);
        assert_eq!(tok("0o7_7"), vec![Token::Int(0o77)]);
    }

    #[test]
    fn test_octal_leading_zero() {
        // POSIX-style leading-zero octal: 0755 = 493
        assert_eq!(tok("0755"), vec![Token::Int(0o755)]);
        assert_eq!(tok("0644"), vec![Token::Int(0o644)]);
        assert_eq!(tok("0"), vec![Token::Int(0)]);
    }

    #[test]
    fn test_underscore_separators() {
        assert_eq!(tok("1_000"), vec![Token::Int(1000)]);
        assert_eq!(tok("1_000_000"), vec![Token::Int(1_000_000)]);
        assert_eq!(tok("1_000_000u"), vec![Token::U64Lit(1_000_000)]);
        #[allow(clippy::approx_constant)]
        {
            // 1_000.5 → Float(1000.5)
            assert_eq!(tok("1_000.5"), vec![Token::Float(1000.5)]);
        }
    }

    #[test]
    fn test_scientific_notation() {
        assert_eq!(tok("1e6"), vec![Token::Float(1_000_000.0)]);
        assert_eq!(tok("1E6"), vec![Token::Float(1_000_000.0)]);
        assert_eq!(tok("1.5e3"), vec![Token::Float(1500.0)]);
        assert_eq!(tok("1.5e-3"), vec![Token::Float(0.0015)]);
        assert_eq!(tok("1.5e+3"), vec![Token::Float(1500.0)]);
        // Negative base with scientific notation
        assert_eq!(tok("-1e6"), vec![Token::Float(-1_000_000.0)]);
    }

    #[test]
    fn test_u64_suffix() {
        assert_eq!(tok("42u"), vec![Token::U64Lit(42)]);
        assert_eq!(tok("0u"), vec![Token::U64Lit(0)]);
        // u64 max fits
        assert_eq!(tok("18446744073709551615u"), vec![Token::U64Lit(u64::MAX)]);
    }

    #[test]
    fn test_number_error_invalid_digit() {
        // Binary: 2 is invalid
        assert!(tok_err("0b2").contains("invalid digit `2` in binary literal"));
        // Octal: 9 is invalid
        assert!(tok_err("0o9").contains("invalid digit `9` in octal literal"));
        // Octal: 8 is invalid
        assert!(tok_err("0789").contains("invalid digit `8` in octal literal"));
        // Hex with invalid char is ok — 'g' is invalid
        assert!(tok_err("0xg").contains("invalid digit"));
    }

    #[test]
    fn test_number_error_trailing_chars() {
        // Trailing alphabetic chars that are not a valid suffix
        assert!(tok_err("42abc").contains("unexpected character `a` after number"));
        // 42w — 'w' is not 'u' suffix or 'e'/'E' exponent
        assert!(tok_err("42w").contains("unexpected character `w` after number"));
        // 0w — octal-looking but 'w' is invalid prefix
        assert!(tok_err("0w0").contains("unexpected character `w` after number"));
    }

    #[test]
    fn test_number_error_trailing_underscore() {
        // Trailing underscore is invalid
        assert!(tok_err("1_").contains("underscore must be followed by a digit"));
        // Double underscore
        assert!(tok_err("1__0").contains("underscore must be followed by a digit"));
    }

    #[test]
    fn test_number_error_radix_no_digits() {
        // Radix prefix with no following digits
        assert!(tok_err("0x").contains("expected digits after `0x`"));
        assert!(tok_err("0b").contains("expected digits after `0b`"));
        assert!(tok_err("0o").contains("expected digits after `0o`"));
    }

    #[test]
    fn test_number_error_negative_u64() {
        // Negative u64 is invalid
        assert!(tok_err("-1u").contains("`u` suffix cannot be used with negative numbers"));
    }

    #[test]
    fn test_number_error_float_u_suffix() {
        // Float with u suffix is invalid
        assert!(tok_err("1.5u").contains("`u` suffix is not valid on float literals"));
    }

    #[test]
    fn test_booleans() {
        // true/false are now plain identifiers — Boolean is a user-defined type (Boolean: [type True False])
        assert_eq!(tok("true"), vec![Token::Identifier("true".into())]);
        assert_eq!(tok("false"), vec![Token::Identifier("false".into())]);
        assert_eq!(
            tok("true false"),
            vec![
                Token::Identifier("true".into()),
                Token::Identifier("false".into())
            ]
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
        // Access field names use the same denylist as general identifiers, so !
        // is allowed — "foo!bar" is a single identifier token.
        assert_eq!(
            tok("$a.foo!bar"),
            vec![
                Token::EscapedRef("a".into()),
                Token::Dot,
                Token::Identifier("foo!bar".into()),
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
        // 1e309 is a float literal with scientific notation. The value overflows f64 to infinity.
        // The lexer parses it as a float; downstream code may reject infinities.
        let tokens = tok("1e309");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0], Token::Float(f64::INFINITY));
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
            tok("x@Integer"),
            vec![
                Token::Identifier("x".into()),
                Token::ImmediateAt,
                Token::Identifier("Integer".into())
            ]
        );

        // Identifier followed by @ with whitespace — At
        assert_eq!(
            tok("x @Integer"),
            vec![
                Token::Identifier("x".into()),
                Token::At,
                Token::Identifier("Integer".into())
            ]
        );

        // EscapedRef followed by @ — At (not ImmediateAt, only fires after Identifier)
        assert_eq!(
            tok("$var@Integer"),
            vec![
                Token::EscapedRef("var".into()),
                Token::At,
                Token::Identifier("Integer".into())
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
