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
    /// `[` immediately after a value (no whitespace gap) — bracket access
    BracketAccess,
    /// `]` (closing bracket)
    CloseBracket,
    /// `:` (key-value separator)
    Colon,
    /// `;` (optional entry separator)
    Semicolon,
    /// `.` (dot access operator, only in access context)
    Dot,
    /// `..` (range operator, only in bracket-access context)
    Range,
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
    /// Bare word (unquoted string)
    BareWord(String),
    /// Quoted string literal (escapes already processed)
    QuotedString(String),
    /// Variable reference `$name`
    VarRef(String),
    /// Boolean literal (`true` or `false`)
    BoolLit(bool),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::OpenBracket => write!(f, "["),
            Token::BracketAccess => write!(f, "["),
            Token::CloseBracket => write!(f, "]"),
            Token::Colon => write!(f, ":"),
            Token::Semicolon => write!(f, ";"),
            Token::Dot => write!(f, "."),
            Token::Range => write!(f, ".."),
            Token::At => write!(f, "@"),
            Token::ImmediateAt => write!(f, "@"),
            Token::Ellipsis => write!(f, "..."),
            Token::DocSeparator => write!(f, "---"),
            Token::Newline => write!(f, "\\n"),
            Token::Comment(text) => write!(f, "# {text}"),
            Token::Int(n) => write!(f, "{n}"),
            Token::Float(n) => write!(f, "{n}"),
            Token::BareWord(s) => write!(f, "{s}"),
            Token::QuotedString(s) => write!(f, "\"{s}\""),
            Token::VarRef(name) => write!(f, "${name}"),
            Token::BoolLit(b) => write!(f, "{b}"),
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
    /// - `$a.b` (no gap) → dot access; `$a .b` (gap) → two separate tokens
    /// - `word@annotation` (no gap) → ImmediateAt; `word @annotation` (gap) → At
    /// - `$a[0]` (no gap) → BracketAccess; `$a [0]` (gap) → OpenBracket
    ///
    /// This flag is reset at the start of each token loop iteration and set by `skip_whitespace_except_newline()`.
    had_whitespace_before: bool,
    /// Bracket nesting depth (for range operator context and MAX_LEX_DEPTH enforcement)
    bracket_depth: usize,
    /// True if the last token was Dot in an access chain (next bare word excludes dots)
    after_access_dot: bool,
    /// Tracks the last significant token type for O(1) access context detection.
    ///
    /// Used to determine when `[` should emit `BracketAccess` (immediately after a value-ending token
    /// with no whitespace gap) vs `OpenBracket`. Value-ending tokens: VarRef, CloseBracket, BareWord,
    /// QuotedString, Int, Float, BoolLit. This enables `$a[0]` (bracket access) vs `$a [0]` (separate tokens).
    last_significant_token: Option<LastSignificantToken>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LastSignificantToken {
    VarRef,
    CloseBracket,
    BareWordAfterDot,
    QuotedString,
    Int,
    Float,
    BoolLit,
    BareWord,
    Other,
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
            last_significant_token: None,
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
                    // Handle CRLF. A newline (even as part of CRLF) acts as whitespace
                    // for bracket-access detection: `[` on the next line is never a
                    // BracketAccess on the previous line's value.
                    self.had_whitespace_before = true;
                    self.last_significant_token = None;
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
                    // A newline acts as whitespace for bracket-access detection: `[` on
                    // the next line is never a BracketAccess on the previous line's value.
                    self.had_whitespace_before = true;
                    self.last_significant_token = None;
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
                // Comments don't update last_significant_token
                self.lex_comment()
            }
            '[' => {
                self.after_access_dot = false;
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

                // Emit BracketAccess if no whitespace before and previous token is value-ending
                let token = if !self.had_whitespace_before && self.is_bracket_access_context() {
                    Token::BracketAccess
                } else {
                    Token::OpenBracket
                };

                self.tokens.push(Spanned::new(token, Span::new(start, end)));
                self.last_significant_token = Some(LastSignificantToken::Other);
                Ok(())
            }
            ']' => {
                self.after_access_dot = false;
                self.advance();
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                // Note: Lexer doesn't validate bracket matching (parser's job)
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::CloseBracket, Span::new(start, end)));
                self.last_significant_token = Some(LastSignificantToken::CloseBracket);
                Ok(())
            }
            ':' => {
                self.after_access_dot = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Colon, Span::new(start, end)));
                self.last_significant_token = Some(LastSignificantToken::Other);
                Ok(())
            }
            ';' => {
                self.after_access_dot = false;
                self.advance();
                let end = self.current_position();
                self.tokens
                    .push(Spanned::new(Token::Semicolon, Span::new(start, end)));
                self.last_significant_token = Some(LastSignificantToken::Other);
                Ok(())
            }
            '@' => {
                self.after_access_dot = false;
                self.advance();
                let end = self.current_position();

                // Emit ImmediateAt if no whitespace before and previous token is BareWord
                let token = if !self.had_whitespace_before && self.is_immediate_at_context() {
                    Token::ImmediateAt
                } else {
                    Token::At
                };

                self.tokens.push(Spanned::new(token, Span::new(start, end)));
                self.last_significant_token = Some(LastSignificantToken::Other);
                Ok(())
            }
            '"' => {
                self.after_access_dot = false;
                self.lex_quoted_string()
            }
            '$' => {
                self.after_access_dot = false;
                self.lex_var_ref()
            }
            '%' => {
                // % starts a pipeline-variable bare word.
                // Lexed like a VarRef (stops at '.' so %.x → BareWord("%") Dot BareWord("x")),
                // but emits a BareWord (not a VarRef) because % is not a $ sigil.
                self.after_access_dot = false;
                self.lex_percent_word()
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
                // Check for ellipsis, range, or bare word
                if self.peek_ahead(1) == Some('.') {
                    if self.peek_ahead(2) == Some('.') {
                        // Ellipsis
                        self.after_access_dot = false;
                        self.advance();
                        self.advance();
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Ellipsis, Span::new(start, end)));
                        self.last_significant_token = Some(LastSignificantToken::Other);
                        Ok(())
                    } else {
                        // Could be range or bare word
                        // Range is only valid inside brackets
                        if self.bracket_depth > 0 {
                            self.after_access_dot = false;
                            self.advance();
                            self.advance();
                            let end = self.current_position();
                            self.tokens
                                .push(Spanned::new(Token::Range, Span::new(start, end)));
                            self.last_significant_token = Some(LastSignificantToken::Other);
                            Ok(())
                        } else {
                            // Bare word - after_access_dot handled in lex_bare_word_or_number
                            self.lex_bare_word_or_number()
                        }
                    }
                } else {
                    // Check if this is dot access (after VarRef, CloseBracket, or BareWord following Dot)
                    // Only if NO whitespace preceded the dot
                    if !self.had_whitespace_before && self.is_access_context() {
                        self.advance();
                        let end = self.current_position();
                        self.tokens
                            .push(Spanned::new(Token::Dot, Span::new(start, end)));
                        self.after_access_dot = true;
                        // Dot doesn't update last_significant_token (it's an operator, not a value)
                        Ok(())
                    } else {
                        // Bare word - after_access_dot handled in lex_bare_word_or_number
                        self.lex_bare_word_or_number()
                    }
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
                ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '$'
            )
        } else {
            false
        }
    }

    fn is_access_context(&self) -> bool {
        // Dot is an access operator if the previous significant token was:
        // - VarRef
        // - CloseBracket
        // - BareWord that followed a Dot (making this part of a chain like $a.b.c)
        matches!(
            self.last_significant_token,
            Some(LastSignificantToken::VarRef)
                | Some(LastSignificantToken::CloseBracket)
                | Some(LastSignificantToken::BareWordAfterDot)
        )
    }

    fn is_bracket_access_context(&self) -> bool {
        // BracketAccess is emitted instead of OpenBracket when `[` follows
        // a value-ending token with no whitespace gap.
        // Value-ending tokens: VarRef, CloseBracket, BareWordAfterDot, QuotedString, Int, Float, BoolLit
        matches!(
            self.last_significant_token,
            Some(LastSignificantToken::VarRef)
                | Some(LastSignificantToken::CloseBracket)
                | Some(LastSignificantToken::BareWordAfterDot)
                | Some(LastSignificantToken::QuotedString)
                | Some(LastSignificantToken::Int)
                | Some(LastSignificantToken::Float)
                | Some(LastSignificantToken::BoolLit)
        )
    }

    fn is_immediate_at_context(&self) -> bool {
        // ImmediateAt is emitted instead of At when `@` follows
        // a BareWord with no whitespace gap.
        matches!(
            self.last_significant_token,
            Some(LastSignificantToken::BareWord)
        )
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
        // Comments don't update last_significant_token (they're ignored for access context)
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
                    self.last_significant_token = Some(LastSignificantToken::QuotedString);
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
            .push(Spanned::new(Token::VarRef(name), Span::new(start, end)));
        self.last_significant_token = Some(LastSignificantToken::VarRef);
        Ok(())
    }

    /// Lex a `%`-prefixed pipeline variable bare word.
    ///
    /// Reads `%` plus identifier characters (stopping at `.` so that `%.x` tokenises as
    /// `BareWord("%")`, `Dot`, `BareWord("x")` rather than `BareWord("%.x")`).
    /// Emits `BareWord` (not `VarRef`) because `%` is not a `$` sigil.
    /// After emission, updates `last_significant_token` to `VarRef` so that a following
    /// `.` without whitespace is recognised as a dot-access operator.
    fn lex_percent_word(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        let word_start = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        // Consume '%'
        self.advance();
        // Consume identifier characters (same denylist as VarRef, stops at '.')
        while let Some(c) = self.peek_char() {
            if self.is_var_ident_char(c) {
                self.advance();
            } else {
                break;
            }
        }
        let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let name = self.input[word_start..word_end].to_string();
        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::BareWord(name), Span::new(start, end)));
        // Set VarRef so a following '.' without whitespace is recognised as dot-access.
        self.last_significant_token = Some(LastSignificantToken::VarRef);
        Ok(())
    }

    fn is_var_ident_char(&self, c: char) -> bool {
        // Denylist: exclude whitespace, structural delimiters, and dot (access operator)
        !matches!(
            c,
            ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '.'
        )
    }

    fn lex_doc_separator(&mut self) -> Result<(), LexError> {
        let start = self.current_position();
        self.advance();
        self.advance();
        self.advance();
        let end = self.current_position();
        self.tokens
            .push(Spanned::new(Token::DocSeparator, Span::new(start, end)));
        self.last_significant_token = Some(LastSignificantToken::Other);
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
                return self.lex_number(start, word_start);
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
                // Normal bare word, dots are allowed
                if self.is_bare_word_char(c) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        let word_end = self.current.map(|(i, _)| i).unwrap_or(self.input.len());
        let word = self.input[word_start..word_end].to_string();
        let end = self.current_position();

        // Check for boolean literals
        if word == "true" {
            self.tokens
                .push(Spanned::new(Token::BoolLit(true), Span::new(start, end)));
            self.last_significant_token = Some(LastSignificantToken::BoolLit);
        } else if word == "false" {
            self.tokens
                .push(Spanned::new(Token::BoolLit(false), Span::new(start, end)));
            self.last_significant_token = Some(LastSignificantToken::BoolLit);
        } else {
            self.tokens
                .push(Spanned::new(Token::BareWord(word), Span::new(start, end)));
            // BareWord after Dot is in access context for chaining
            if in_access_field {
                self.last_significant_token = Some(LastSignificantToken::BareWordAfterDot);
            } else {
                self.last_significant_token = Some(LastSignificantToken::BareWord);
            }
        }
        Ok(())
    }

    fn is_access_field_char(&self, c: char, is_first: bool) -> bool {
        // Access field names (after dot in access chain) use allowlist from grammar
        // Based on grammar: access_field = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }
        if is_first {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '?'
        }
    }

    fn lex_number(&mut self, start: Position, word_start: usize) -> Result<(), LexError> {
        // Collect digits
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float (decimal point followed by digits)
        if self.peek_char() == Some('.') {
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
                        self.last_significant_token = Some(LastSignificantToken::Float);
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
                        self.last_significant_token = Some(LastSignificantToken::Int);
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
                    self.last_significant_token = Some(LastSignificantToken::Int);
                    Ok(())
                }
                Err(e) => Err(LexError::new(
                    format!("invalid integer literal: {e}"),
                    Span::new(start, end),
                )),
            }
        }
    }

    fn is_bare_word_char(&self, c: char) -> bool {
        // Denylist: exclude whitespace, structural delimiters, and $
        !matches!(
            c,
            ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '$'
        )
    }
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
        assert_eq!(tok("3.14"), vec![Token::Float(3.14)]);
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
        assert_eq!(tok("hello"), vec![Token::BareWord("hello".into())]);
        assert_eq!(tok("my-var"), vec![Token::BareWord("my-var".into())]);
        assert_eq!(tok("has?"), vec![Token::BareWord("has?".into())]);
        assert_eq!(tok("file.txt"), vec![Token::BareWord("file.txt".into())]);
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
        assert_eq!(tok("$x"), vec![Token::VarRef("x".into())]);
        assert_eq!(tok("$my-var"), vec![Token::VarRef("my-var".into())]);
        assert_eq!(tok("$$"), vec![Token::VarRef("$".into())]);
        assert_eq!(tok("$$foo"), vec![Token::VarRef("$foo".into())]);
        assert_eq!(tok("$0"), vec![Token::VarRef("0".into())]);
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
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("b".into())
            ]
        );
        assert_eq!(
            tok("$a.b.c"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("b".into()),
                Token::Dot,
                Token::BareWord("c".into())
            ]
        );
        // Access field names only allow [a-zA-Z0-9_-?] per grammar
        assert_eq!(
            tok("$a.foo!bar"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("foo".into()),
                Token::BareWord("!bar".into())
            ]
        );
    }

    #[test]
    fn test_whitespace_sensitivity() {
        // $a.b is dot access
        assert_eq!(
            tok("$a.b"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("b".into())
            ]
        );

        // $a .b is VarRef then bare word
        assert_eq!(
            tok("$a .b"),
            vec![Token::VarRef("a".into()), Token::BareWord(".b".into())]
        );

        // a.b is bare word (no $ prefix)
        assert_eq!(tok("a.b"), vec![Token::BareWord("a.b".into())]);
    }

    #[test]
    fn test_bracket_access() {
        // VarRef precedes — BracketAccess (no whitespace)
        assert_eq!(
            tok("$a[0]"),
            vec![
                Token::VarRef("a".into()),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // VarRef precedes with whitespace — OpenBracket
        assert_eq!(
            tok("$a [0]"),
            vec![
                Token::VarRef("a".into()),
                Token::OpenBracket,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // CloseBracket precedes — BracketAccess
        assert_eq!(tok("]["), vec![Token::CloseBracket, Token::BracketAccess]);

        // QuotedString precedes — BracketAccess
        assert_eq!(
            tok(r#""str"[0]"#),
            vec![
                Token::QuotedString("str".into()),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // Int precedes — BracketAccess
        assert_eq!(
            tok("42[0]"),
            vec![
                Token::Int(42),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // Float precedes — BracketAccess
        assert_eq!(
            tok("3.14[0]"),
            vec![
                Token::Float(3.14),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // BoolLit precedes — BracketAccess
        assert_eq!(
            tok("true[0]"),
            vec![
                Token::BoolLit(true),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // BareWord with whitespace — OpenBracket
        assert_eq!(
            tok("call [x]"),
            vec![
                Token::BareWord("call".into()),
                Token::OpenBracket,
                Token::BareWord("x".into()),
                Token::CloseBracket
            ]
        );

        // Call expression followed by bracket access — BracketAccess
        assert_eq!(
            tok("[call $f][0]"),
            vec![
                Token::OpenBracket,
                Token::BareWord("call".into()),
                Token::VarRef("f".into()),
                Token::CloseBracket,
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );

        // BareWordAfterDot precedes — BracketAccess (real usage: $a.b[0])
        assert_eq!(
            tok("$a.b[0]"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("b".into()),
                Token::BracketAccess,
                Token::Int(0),
                Token::CloseBracket
            ]
        );
    }

    #[test]
    fn test_range_operator() {
        // Range in bracket access context
        assert_eq!(
            tok("$a[2..5]"),
            vec![
                Token::VarRef("a".into()),
                Token::BracketAccess,
                Token::Int(2),
                Token::Range,
                Token::Int(5),
                Token::CloseBracket
            ]
        );

        // .. outside bracket access is bare word
        assert_eq!(tok("file..bak"), vec![Token::BareWord("file..bak".into())]);
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
                Token::BareWord("a".into()),
                Token::Newline,
                Token::BareWord("b".into())
            ]
        );
    }

    #[test]
    fn test_newline_prevents_bracket_access() {
        // A newline between `]` and `[` must produce OpenBracket, not BracketAccess.
        // `]\n[` is two separate expressions (the `\n` is treated as whitespace for
        // access-context purposes). Without this, `[x: 42]\n[y: $x]` would parse the
        // second dict as a bracket access on the first dict rather than a new expression.
        assert_eq!(
            tok("]\n["),
            vec![Token::CloseBracket, Token::Newline, Token::OpenBracket]
        );
        // CRLF also prevents bracket access
        assert_eq!(
            tok("]\r\n["),
            vec![Token::CloseBracket, Token::Newline, Token::OpenBracket]
        );
        // Same-line `][` still produces BracketAccess
        assert_eq!(tok("]["), vec![Token::CloseBracket, Token::BracketAccess]);
    }

    #[test]
    fn test_crlf() {
        assert_eq!(tok("\r\n"), vec![Token::Newline]);
        assert_eq!(
            tok("a\r\nb"),
            vec![
                Token::BareWord("a".into()),
                Token::Newline,
                Token::BareWord("b".into())
            ]
        );
    }

    #[test]
    fn test_doc_separator() {
        assert_eq!(tok("---"), vec![Token::DocSeparator]);
        assert_eq!(tok("--- "), vec![Token::DocSeparator]);
        assert_eq!(tok("----"), vec![Token::BareWord("----".into())]);
    }

    #[test]
    fn test_percent_bare_words() {
        // % is a valid bare_word_char, so %defaults lexes as BareWord
        assert_eq!(tok("%defaults"), vec![Token::BareWord("%defaults".into())]);
        assert_eq!(tok("%"), vec![Token::BareWord("%".into())]);
        assert_eq!(tok("%+"), vec![Token::BareWord("%+".into())]);
        // %name should also work with various characters
        assert_eq!(tok("%config"), vec![Token::BareWord("%config".into())]);
        assert_eq!(tok("%raw_data"), vec![Token::BareWord("%raw_data".into())]);
    }

    #[test]
    fn test_percent_word_dot_access() {
        // lex_percent_word stops at '.' and sets last_significant_token = VarRef so
        // that the immediately-following '.' (no whitespace) is recognised as Dot
        // (the dot-access operator) rather than starting a bare word.
        // %base.x  →  BareWord("%base"), Dot, BareWord("x")
        assert_eq!(
            tok("%base.x"),
            vec![
                Token::BareWord("%base".into()),
                Token::Dot,
                Token::BareWord("x".into()),
            ]
        );
        // Chained access: %cfg.server.port
        assert_eq!(
            tok("%cfg.server.port"),
            vec![
                Token::BareWord("%cfg".into()),
                Token::Dot,
                Token::BareWord("server".into()),
                Token::Dot,
                Token::BareWord("port".into()),
            ]
        );
        // Whitespace before dot: NOT dot-access — separate tokens
        assert_eq!(
            tok("%base .x"),
            vec![
                Token::BareWord("%base".into()),
                Token::BareWord(".x".into()),
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
                Token::BareWord("call".into()),
                Token::VarRef("f".into()),
                Token::BareWord("x".into()),
                Token::Colon,
                Token::Int(42),
                Token::CloseBracket
            ]
        );
    }

    #[test]
    fn test_ellipsis_vs_range() {
        assert_eq!(tok("..."), vec![Token::Ellipsis]);
        assert_eq!(
            tok("$a[..]"),
            vec![
                Token::VarRef("a".into()),
                Token::BracketAccess,
                Token::Range,
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
        assert_eq!(result.len(), 3); // BareWord, Newline, BareWord
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
        assert_eq!(tokens[1], Token::BareWord("e309".into()));
    }

    #[test]
    fn test_access_field_first_char() {
        // Access field must start with ASCII_ALPHA or '_' per grammar
        // $a.1 should tokenize as VarRef, Dot, Int (not an access chain)
        assert_eq!(
            tok("$a.1"),
            vec![Token::VarRef("a".into()), Token::Dot, Token::Int(1)]
        );

        // $a._priv should work (underscore is allowed as first char)
        assert_eq!(
            tok("$a._priv"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("_priv".into())
            ]
        );

        // $a.-foo should not work (hyphen not allowed as first char)
        // After dot, next char is '-', which is not alphabetic or '_',
        // so access field ends immediately and '-foo' is separate
        assert_eq!(
            tok("$a.-foo"),
            vec![
                Token::VarRef("a".into()),
                Token::Dot,
                Token::BareWord("-foo".into())
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
        // BareWord followed by @ with no whitespace — ImmediateAt
        assert_eq!(
            tok("x@Int"),
            vec![
                Token::BareWord("x".into()),
                Token::ImmediateAt,
                Token::BareWord("Int".into())
            ]
        );

        // BareWord followed by @ with whitespace — At
        assert_eq!(
            tok("x @Int"),
            vec![
                Token::BareWord("x".into()),
                Token::At,
                Token::BareWord("Int".into())
            ]
        );

        // VarRef followed by @ — At (not ImmediateAt, only fires after BareWord)
        assert_eq!(
            tok("$var@Int"),
            vec![
                Token::VarRef("var".into()),
                Token::At,
                Token::BareWord("Int".into())
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
}
