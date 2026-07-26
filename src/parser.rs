//! Iterative parser for tinct.
//! unified-bindings invariant: [fn [let ...] body] required; [fn [x y] body] is a parse error.
//!
//! Hand-written recursive-descent parser that replaced the pest-based parser in sprint parser-core-c3.
//! Implements all language constructs: call/fn/type-alias/type-assert forms, keyed entries,
//! dot access chains (identifier and integer keys), pipe expressions, document separators,
//! comment collection, and fn param list parsing (simple, annotated, variadic).
//!
//! The parser enforces a maximum nesting depth of 256 brackets to prevent stack overflow.
//! Unlike the previous pest parser (which recursed on Rust's call stack), this implementation
//! uses an explicit stack for bracket nesting, making it safe for deeply nested inputs.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ast::*;
use crate::ast::{
    SurfaceDeclaration, SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceItem,
    SurfaceMatchArm, SurfaceNamedArg, SurfaceNode, SurfaceParam, SurfaceProgram,
};
use crate::error::TypeDiagnostic;
use crate::lexer::{self, Token};

/// Parser-local shorthand: create an `Arc<SurfaceNode>` with fresh inline annotations.
/// Equivalent to `Arc::new(SurfaceNode::new(expr, span))`.
#[inline(always)]
fn mk(expr: SurfaceExpression, span: Span) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(expr, span))
}

/// Parser-internal error type: message and optional source location.
///
/// Private to this module. At public API boundaries (parse(), parse_surface_expression(),
/// format_parse_error(), ParseOutput.diagnostics) this is converted to `TypeDiagnostic`
/// via the `From<ParseError>` impl (which enables `?` propagation) or by explicit
/// `TypeDiagnostic::error(...)` calls with a context note.
#[derive(Debug, Clone, PartialEq, Default)]
struct ParseError {
    message: String,
    span: Option<Span>,
    /// Optional help suggestion rendered as `= help: ...` in the formatted output.
    help: Option<&'static str>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            write!(
                f,
                "{}:{}: {}",
                span.start_line, span.start_col, self.message
            )
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ParseError {}

/// `From<ParseError> for TypeDiagnostic` — enables `?` and `return Err(e)` within `parse()`.
///
/// When the error has no span, uses a Rust-source span via `rust_span!()` as a fallback.
/// This impl is private to the parser module (ParseError is a private type).
impl From<ParseError> for TypeDiagnostic {
    fn from(err: ParseError) -> Self {
        let span = err.span.unwrap_or_else(|| crate::rust_span!());
        let mut diag = TypeDiagnostic::error("parse-error", err.message, span);
        if let Some(help) = err.help {
            diag.add_help(help);
        }
        diag
    }
}

/// Maximum nesting depth for bracket expressions (enforced before allocation).
const MAX_PARSE_DEPTH: usize = 256;

/// Helper: peek at the next significant (non-whitespace, non-newline, non-comment) token.
fn peek_next_significant(
    tokens: &[Spanned<Token>],
    current_index: usize,
) -> Option<(&Token, usize)> {
    let mut idx = current_index + 1;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Newline | Token::Semicolon | Token::Comment(_) => {
                idx += 1;
            }
            token => return Some((token, idx)),
        }
    }
    None
}

/// Helper: peek at the next horizontally adjacent token (skip comments only, but NOT newlines or semicolons).
/// Used for keyword-colon lookahead where `[call\n: x]` and `[call;: x]` must NOT be classified as dict entries.
fn peek_next_horizontal(
    tokens: &[Spanned<Token>],
    current_index: usize,
) -> Option<(&Token, usize)> {
    let mut idx = current_index + 1;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Comment(_) => {
                idx += 1;
            }
            token => return Some((token, idx)),
        }
    }
    None
}

/// Extract a comparable string from a key expression for duplicate detection.
/// Returns None for complex expressions where comparison isn't meaningful.
///
/// Parse-time duplicate detection is literal-keys-only; computed keys (Field,
/// Call) return None here and are checked at eval-time.
fn key_to_string(expr: &SurfaceExpression) -> Option<String> {
    match expr {
        SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
        SurfaceExpression::Int(n) => Some(n.to_string()),
        SurfaceExpression::U64(n) => Some(n.to_string()),
        SurfaceExpression::Float(n) => Some(n.to_string()),
        // Bare identifier keys are normalized to StringLiteral in push_value before
        // key_to_string is called. Escaped VarRef keys ($foo:) are computed keys whose
        // string representation isn't known at parse time → None.
        _ => None,
    }
}

/// Encode a span position as a `u64` key for comment maps.
///
/// Combines `start_line` and `start_col` into a single `u64`:
/// `(start_line as u64) << 32 | start_col as u64`.
///
/// This is a unique key within a single source file because no two tokens can
/// start at the same line and column. Keys are used by `leading_comments`,
/// `trailing_comments`, and `blank_before` maps in `ParseOutput`.
#[inline]
pub(crate) fn span_key(line: u32, col: u32) -> u64 {
    ((line as u64) << 32) | col as u64
}

/// Helper: count how many whitespace/newline/semicolon tokens to skip from the current position.
/// Also collects comment tokens into the leading_comments map (keyed by span_key of the next non-whitespace token).
/// Detects blank lines (consecutive newlines) and marks the next token with blank_before: true.
fn skip_whitespace_tokens(
    tokens: &[Spanned<Token>],
    current_index: usize,
    leading_comments: &mut BTreeMap<u64, Vec<String>>,
    blank_before: &mut BTreeMap<u64, bool>,
) -> usize {
    let mut count = 0;
    let mut idx = current_index;
    let mut collected_comments = Vec::new();
    let mut consecutive_newlines = 0;
    let mut has_blank_line = false;

    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Comment(text) => {
                collected_comments.push(text.clone());
                consecutive_newlines = 0; // Reset on comment
                count += 1;
                idx += 1;
            }
            Token::Newline => {
                consecutive_newlines += 1;
                if consecutive_newlines >= 2 {
                    has_blank_line = true;
                }
                count += 1;
                idx += 1;
            }
            Token::Semicolon => {
                consecutive_newlines += 1; // Semicolon acts as newline alias for blank-line detection
                if consecutive_newlines >= 2 {
                    has_blank_line = true;
                }
                count += 1;
                idx += 1;
            }
            _ => {
                // Found non-whitespace token — attach collected comments and blank-line flag to it
                if idx < tokens.len() {
                    let next_key =
                        span_key(tokens[idx].span.start_line, tokens[idx].span.start_col);
                    if !collected_comments.is_empty() {
                        leading_comments
                            .entry(next_key)
                            .or_default()
                            .extend(collected_comments);
                    }
                    if has_blank_line {
                        blank_before.insert(next_key, true);
                    }
                }
                break;
            }
        }
    }
    count
}

/// Parse an annotation directly from the token stream for header contexts.
///
/// Starts at `start_index` which must be `Token::At` or `Token::ImmediateAt`.
/// Returns `(Annotation, next_index)` on success.
///
/// Handles:
/// - `@Name` → `Annotation::Simple("Name")` (via expression_to_annotation)
/// - `@Expr` → `Annotation::Quote` (via expression_to_annotation)
/// - `@Name@Inner` → `Annotation::Annotated(Simple("Name"), inner)` (T-1618: outer is Box<Annotation>)
/// - `@[key: val ...]` → `Annotation::PropertyDict(entries)` (via expression_to_annotation)
/// - `@[key: val ...]@Next` → `Annotation::Annotated(PropertyDict(...), Next)` (T-1618)
///
/// The `@Name` and `@[...]` cases build a `SurfaceNode` (VarRef or Dict) and delegate to
/// `expression_to_annotation` for the actual annotation conversion — the same function used
/// in the main body annotation path. Bracket parsing for `@[...]` delegates to the extracted
/// `parse_bracket_annotation_dict` helper (T-1778) to avoid duplication. This function is
/// necessary because header parsing occurs outside the main iterative parser's bracket-nesting
/// stack machine; there is no mechanism to invoke the main Dict frame as a sub-routine from
/// header context.

/// Parse a bracket annotation dict `[key: val ...]` starting from the `[` token.
///
/// Extracted from `parse_annotation_direct` to eliminate token-scanning loop duplication (T-1778).
/// This function parses the contents of `@[...]` annotations, handling a restricted subset of
/// tokens suitable for property dict annotations (Identifier, StringLiteral, Int, Float, Colon).
///
/// Returns `(entries, annotation_span, final_token_index)` on success, where `final_token_index`
/// points to the token immediately after the closing `]`.
///
/// # Parameters
/// - `tokens`: full token slice
/// - `start_index`: index of the `OpenBracket` token
/// - `leading_comments`, `blank_before`: comment maps for whitespace token skipping
///
/// # Behavior
/// - Consumes the opening `[` at `start_index`
/// - Parses `key: value` pairs and positional entries
/// - Tracks nesting depth for nested brackets (treated as opaque in values)
/// - Returns error on unclosed brackets or malformed entries (e.g., trailing `:`)
fn parse_bracket_annotation_dict(
    tokens: &[Spanned<Token>],
    start_index: usize,
    leading_comments: &mut BTreeMap<u64, Vec<String>>,
    blank_before: &mut BTreeMap<u64, bool>,
) -> Result<(Vec<Spanned<SurfaceEntry>>, Span, usize), ParseError> {
    let bracket_start_span = tokens[start_index].span.clone();
    let mut i = start_index + 1; // consume [

    let mut entries: Vec<Spanned<SurfaceEntry>> = Vec::new();
    let mut depth: usize = 1;
    // True after a key identifier and its `:` have been consumed — the next
    // value token should update the last entry's value, not push a new entry.
    let mut waiting_for_value = false;

    while i < tokens.len() {
        i += skip_whitespace_tokens(tokens, i, leading_comments, blank_before);
        if i >= tokens.len() {
            break;
        }

        match &tokens[i].node {
            Token::CloseBracket => {
                depth -= 1;
                if depth == 0 {
                    // If we were waiting for a value after a colon, the annotation is
                    // malformed (e.g. `@[type:]`). The last entry has a stale value node.
                    // Treat as a parse error — a trailing colon in a property dict
                    // annotation is not valid syntax.
                    if waiting_for_value {
                        return Err(ParseError {
                            message: "missing value after `:` in property dict annotation"
                                .to_string(),
                            span: Some(tokens[i].span.clone()),
                            help: None,
                        });
                    }

                    let ann_span = Span::new(
                        bracket_start_span.start_line,
                        bracket_start_span.start_col,
                        tokens[i].span.end_line,
                        tokens[i].span.end_col,
                        Arc::clone(&bracket_start_span.file),
                    );
                    let final_i = i + 1; // consume ]
                    return Ok((entries, ann_span, final_i));
                }
                // Nested bracket close inside a value — this shouldn't happen in the
                // simple key: value annotation dict, but handle gracefully.
                i += 1;
            }
            Token::OpenBracket => {
                // Nested bracket in annotation value — skip (treat as opaque).
                depth += 1;
                i += 1;
            }
            Token::Colon => {
                // Key separator: promote the last entry's value node to a key.
                // The key node must be StringLiteral so that get_property() can find it
                // (get_property matches SurfaceExpression::StringLiteral { content }).
                if let Some(last_entry) = entries.last_mut() {
                    if last_entry.node.key.is_none() {
                        // Extract the identifier name from the VarRef value node.
                        let key_str = match &last_entry.node.value.expr {
                            SurfaceExpression::VarRef { name, .. } => name.clone(),
                            SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                            _ => String::new(),
                        };
                        let key_span = last_entry.node.value.span.clone();
                        let key_node = Arc::new(SurfaceNode::new(
                            SurfaceExpression::StringLiteral {
                                prefix: String::new(),
                                delimiter: "\"".to_string(),
                                content: key_str,
                            },
                            key_span.clone(),
                        ));
                        // Temporarily put a placeholder value; will be replaced when the
                        // next value token is consumed.
                        last_entry.node.key = Some(key_node);
                        waiting_for_value = true;
                        i += 1;
                        continue;
                    }
                }
                i += 1; // skip stray colon
            }
            Token::Identifier(name) => {
                let name = name.clone();
                let tok_span = tokens[i].span.clone();
                let node = Arc::new(SurfaceNode::new(
                    SurfaceExpression::VarRef {
                        name,
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                        do_infer_placeholder: false,
                    },
                    tok_span.clone(),
                ));
                if waiting_for_value {
                    // Update the last entry's value in-place.
                    if let Some(last_entry) = entries.last_mut() {
                        last_entry.node.value = node;
                        last_entry.span.end_line = tok_span.end_line;
                        last_entry.span.end_col = tok_span.end_col;
                    }
                    waiting_for_value = false;
                } else {
                    entries.push(Spanned::new(
                        SurfaceEntry {
                            key: None,
                            value: node,
                        },
                        tok_span,
                    ));
                }
                i += 1;
            }
            Token::StringLiteral {
                prefix,
                delimiter,
                content,
            } => {
                let (p, d, c) = (prefix.clone(), delimiter.clone(), content.clone());
                let tok_span = tokens[i].span.clone();
                let node = Arc::new(SurfaceNode::new(
                    SurfaceExpression::StringLiteral {
                        prefix: p,
                        delimiter: d,
                        content: c,
                    },
                    tok_span.clone(),
                ));
                if waiting_for_value {
                    if let Some(last_entry) = entries.last_mut() {
                        last_entry.node.value = node;
                        last_entry.span.end_line = tok_span.end_line;
                        last_entry.span.end_col = tok_span.end_col;
                    }
                    waiting_for_value = false;
                } else {
                    entries.push(Spanned::new(
                        SurfaceEntry {
                            key: None,
                            value: node,
                        },
                        tok_span,
                    ));
                }
                i += 1;
            }
            Token::Int(n) => {
                let n = *n;
                let tok_span = tokens[i].span.clone();
                let node = Arc::new(SurfaceNode::new(
                    SurfaceExpression::Int(n),
                    tok_span.clone(),
                ));
                if waiting_for_value {
                    if let Some(last_entry) = entries.last_mut() {
                        last_entry.node.value = node;
                        last_entry.span.end_line = tok_span.end_line;
                        last_entry.span.end_col = tok_span.end_col;
                    }
                    waiting_for_value = false;
                } else {
                    entries.push(Spanned::new(
                        SurfaceEntry {
                            key: None,
                            value: node,
                        },
                        tok_span,
                    ));
                }
                i += 1;
            }
            Token::Float(f) => {
                let f = *f;
                let tok_span = tokens[i].span.clone();
                let node = Arc::new(SurfaceNode::new(
                    SurfaceExpression::Float(f),
                    tok_span.clone(),
                ));
                if waiting_for_value {
                    if let Some(last_entry) = entries.last_mut() {
                        last_entry.node.value = node;
                        last_entry.span.end_line = tok_span.end_line;
                        last_entry.span.end_col = tok_span.end_col;
                    }
                    waiting_for_value = false;
                } else {
                    entries.push(Spanned::new(
                        SurfaceEntry {
                            key: None,
                            value: node,
                        },
                        tok_span,
                    ));
                }
                i += 1;
            }
            _ => {
                // Unexpected token inside annotation brackets.
                // If we were waiting for a value after `:`, the annotation is malformed.
                if waiting_for_value {
                    return Err(ParseError {
                        message: "missing value after `:` in property dict annotation".to_string(),
                        span: Some(tokens[i].span.clone()),
                        help: None,
                    });
                }
                i += 1;
            }
        }
    }

    Err(ParseError {
        message: "unclosed bracket in property dict annotation".to_string(),
        span: Some(bracket_start_span),
        help: None,
    })
}

fn parse_annotation_direct(
    tokens: &[Spanned<Token>],
    start_index: usize,
    leading_comments: &mut BTreeMap<u64, Vec<String>>,
    blank_before: &mut BTreeMap<u64, bool>,
) -> Result<(Spanned<Annotation>, usize), ParseError> {
    let mut i = start_index;

    // Consume the @ token
    match &tokens[i].node {
        Token::At | Token::ImmediateAt => {
            i += 1;
        }
        _ => {
            return Err(ParseError {
                message: "expected @ to start annotation".to_string(),
                span: Some(tokens[i].span.clone()),
                help: None,
            });
        }
    }

    // Skip whitespace after @
    i += skip_whitespace_tokens(tokens, i, leading_comments, blank_before);

    if i >= tokens.len() {
        // Report at the @ token itself — it was consumed but nothing followed.
        let at_span = tokens[start_index].span.clone();
        return Err(ParseError {
            message: "unexpected end of input after @".to_string(),
            span: Some(at_span),
            help: None,
        });
    }

    match &tokens[i].node {
        Token::Identifier(name) => {
            let name = name.clone();
            let name_span = tokens[i].span.clone();
            i += 1;

            // T-1617: Unify with main body AnnotationCollect path via expression_to_annotation.
            // Build a VarRef SurfaceNode and check for chaining, then delegate conversion.

            // Check for chained annotation: @Name@Inner
            if i < tokens.len() && matches!(&tokens[i].node, Token::ImmediateAt) {
                let (inner_ann, final_i) =
                    parse_annotation_direct(tokens, i, leading_comments, blank_before)?;
                let full_span = Span::new(
                    name_span.start_line,
                    name_span.start_col,
                    inner_ann.span.end_line,
                    inner_ann.span.end_col,
                    Arc::clone(&name_span.file),
                );
                // Build annotated VarRef node, then convert via expression_to_annotation.
                let inner_varref = Arc::new(SurfaceNode::new(
                    SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: Some(inner_ann),
                        do_infer_placeholder: false,
                    },
                    full_span.clone(),
                ));
                return Ok((
                    Spanned::new(expression_to_annotation(&inner_varref), full_span),
                    final_i,
                ));
            }

            // Simple @Name case — route through expression_to_annotation for unification.
            // This handles @Expr → Quote, @Name → Simple(name), exactly as the main body does.
            let varref_node = Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: name.clone(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                    do_infer_placeholder: false,
                },
                name_span.clone(),
            ));
            Ok((
                Spanned::new(expression_to_annotation(&varref_node), name_span),
                i,
            ))
        }
        Token::OpenBracket => {
            // @[key: val ...] property dict annotation — delegate to extracted bracket parser.
            // T-1778: Extracted token-scanning loop to eliminate duplication.
            let bracket_start_span = tokens[i].span.clone();
            let (entries, ann_span, final_i) =
                parse_bracket_annotation_dict(tokens, i, leading_comments, blank_before)?;

            // T-1617: Route through expression_to_annotation for unification
            // with the main body AnnotationCollect path.
            // T-1618: Annotation::Annotated now takes Box<Annotation> as outer,
            // so @[...]@Next is representable. Check for a chained annotation.
            if final_i < tokens.len() && matches!(&tokens[final_i].node, Token::ImmediateAt) {
                let (inner_ann, final_i) =
                    parse_annotation_direct(tokens, final_i, leading_comments, blank_before)?;
                let chained_span = Span::new(
                    bracket_start_span.start_line,
                    bracket_start_span.start_col,
                    inner_ann.span.end_line,
                    inner_ann.span.end_col,
                    Arc::clone(&bracket_start_span.file),
                );
                // Build outer Dict annotation, then chain via Annotated.
                // T-1617: outer uses expression_to_annotation for consistency.
                let outer_dict_node = Arc::new(SurfaceNode::new(
                    SurfaceExpression::Dict(entries),
                    ann_span.clone(),
                ));
                let outer_ann = expression_to_annotation(&outer_dict_node);
                return Ok((
                    Spanned::new(
                        Annotation::Annotated(Box::new(outer_ann), Box::new(inner_ann.node)),
                        chained_span,
                    ),
                    final_i,
                ));
            }
            // T-1617: Build a Dict SurfaceNode and convert via expression_to_annotation.
            let dict_node = Arc::new(SurfaceNode::new(
                SurfaceExpression::Dict(entries),
                ann_span.clone(),
            ));
            Ok((
                Spanned::new(expression_to_annotation(&dict_node), ann_span),
                final_i,
            ))
        }
        _ => Err(ParseError {
            message: format!(
                "expected annotation name or '[' after @, found {:?}",
                tokens[i].node
            ),
            span: Some(tokens[i].span.clone()),
            help: None,
        }),
    }
}

/// Stack frame types for the iterative parser.
///
/// Each variant corresponds to a bracket-form being parsed. The parser pushes
/// a frame on `Token::OpenBracket`, collects entries/args/params
/// during iteration, then pops and constructs the AST node on `Token::CloseBracket`.
#[derive(Debug, Clone)]
enum StackFrame {
    /// Dictionary literal: `[key: value ...]`
    Dict {
        entries: Vec<Spanned<SurfaceEntry>>,
        /// Pending key from an Identifier/StringLiteral/EscapedRef before a colon
        pending_key: Option<Arc<SurfaceNode>>,
        /// Track seen keys for duplicate detection (literal keys only)
        seen_keys: std::collections::HashSet<String>,
        span_start: Span,
        /// Floating annotation: set when `[@Type ...]` form is used; wraps the next value in TypeAssert.
        floating_annotation: Option<Spanned<Annotation>>,
    },
    /// Function call: `[call func arg1 arg2 name: val]` or `[func arg1 arg2 name: val]`
    Call {
        /// For implied calls (`[f x]`), the function is extracted from the head Identifier at frame-push time.
        /// For explicit calls (`[call f x]`), this is None and func is extracted from args[0].
        func: Option<Arc<SurfaceNode>>,
        /// Whether this is an implied call ([f x]) or explicit call ([call f x])
        implied: bool,
        args: Vec<CallArg>,
        /// Pending key for named args (Identifier before colon).
        ///
        /// The third element is the optional annotation from `name@Ann:` syntax — `Some(...)`
        /// when the argument name had a `@[...]` annotation (e.g. `fields@Child:`), `None`
        /// for plain `name:` named arguments.
        pending_key: Option<(String, Span, Option<Spanned<Annotation>>)>,
        span_start: Span,
    },
    /// Function definition: `[fn [let params...] body]` or `[fn@Type [let params...] body]`
    Fn {
        /// Parameter list — parsed from `[fn [let x y] body]` syntax
        params: Vec<Spanned<SurfaceParam>>,
        /// Body expressions — for multi-expression bodies (let-binding)
        body: Vec<Arc<SurfaceNode>>,
        return_ann: Option<Spanned<Annotation>>,
        span_start: Span,
        /// True once the parameter bracket has been consumed (even if it added 0 params).
        /// Prevents a second empty bracket `[]` from being mistaken for a param list.
        params_consumed: bool,
    },
    /// Type alias: `[type expr]` or `[type [params] expr]` or `[type T1 T2 ...]`
    ///
    /// Supports three body forms:
    /// 1. Positional bare uppercase words → unit constructors: `[type Red Green Blue]`
    /// 2. Named uppercase-keyed entries + positional bare uppercase words → payload + unit ctors:
    ///    `[type File: [path: String] Noop]`
    /// 3. Single positional lowercase-keyed dict → structural alias: `[type [port: Int]]`
    TypeAlias {
        /// Type parameters: (name, optional full annotation from @X in [let a@X b]).
        params: Vec<(String, Option<crate::ast::Spanned<crate::ast::Annotation>>)>,
        /// Accumulated type body entries. Each entry is either:
        /// - `key: None` — positional type expression (unit constructor bare word, or structural body)
        /// - `key: Some(k)` — named entry: `k` is the constructor name, value is the payload dict
        type_exprs: Vec<Spanned<SurfaceEntry>>,
        /// Pending key node from a bare word followed by `:` inside [type ...].
        /// Holds the constructor name until the payload value arrives.
        pending_key: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Quote special form: `[quote expr]`
    Quote {
        expr: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Unquote special form: `[unquote expr]` — produces `SurfaceExpression::Unquote`
    Unquote {
        expr: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Unquote-splice special form: `[unquote-splice expr]` — produces `SurfaceExpression::UnquoteSplice`
    UnquoteSplice {
        expr: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Syntax class declaration (macros-v2): `[syntax-class name pattern: [...] message: "..."]`
    SyntaxClass {
        name: Option<String>,
        pattern: Option<Arc<SurfaceNode>>,
        message: Option<String>,
        pending_key: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Match expression: `[match scrutinee pattern: body ...]`
    Match {
        scrutinee: Option<Arc<SurfaceNode>>,
        arms: Vec<SurfaceMatchArm>,
        /// Pending pattern expression (before colon) — may be bracket or identifier
        pending_pattern_expr: Option<Arc<SurfaceNode>>,
        /// Pending pattern (SurfaceNode) paired with optional guard expression after the colon
        pending_pattern: Option<(Arc<SurfaceNode>, Option<Arc<SurfaceNode>>)>,
        span_start: Span,
    },
    /// Class declaration: `[class [param...] [structural-metadata] method: Type ...]`
    /// Name comes from the binding position in the enclosing dict (e.g. `MyClass: [class [let a] ...]`).
    ClassDecl {
        name: Option<String>,
        params: Vec<String>,
        superclasses: Vec<(String, Vec<String>)>,
        methods: Vec<Spanned<SurfaceEntry>>,
        /// Pending key for method entries
        pending_key: Option<Arc<SurfaceNode>>,
        /// Structural metadata bracket (second positional): `[determines: [...] resolver: ... superclasses: ...]`
        structural_metadata: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Instance declaration: `[instance ClassName [pattern [...]]`: methods... ...]`
    InstanceDecl {
        class_name: Option<Arc<SurfaceNode>>,
        /// Match arms: (pattern_expr, method_entries)
        arms: Vec<(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)>,
        /// Pending arm pattern expression (before colon)
        pending_arm_pattern: Option<Arc<SurfaceNode>>,
        /// Pending key for method entries within current arm
        pending_key: Option<Arc<SurfaceNode>>,
        /// Current arm's accumulated method entries (before colon closes the arm)
        current_arm_methods: Vec<Spanned<SurfaceEntry>>,
        span_start: Span,
    },
    /// Pattern declaration: `[pattern [a@Int b@Float]]`
    PatternDecl {
        bindings: Vec<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Binding declaration: `[let x@Int y@Float z: default]`
    /// Used in fn params, class TypeVars, type alias params, instance arm keys, and case arms.
    ///
    /// Colon inside `[let ...]` introduces a default value: `name: default_val`.
    ///
    /// `pending_key` holds the left-hand side node popped when `:` is seen.
    /// - VarRef { name, .. } → single-binding left-hand side
    /// - LetDecl { bindings } → multi-binding group `[a b]`
    LetDecl {
        bindings: Vec<Arc<SurfaceNode>>,
        /// Pending left-hand-side node for `lhs: rhs` syntax.
        /// Holds the last binding popped when `:` is seen; consumed when the rhs is committed.
        pending_key: Option<Arc<SurfaceNode>>,
        /// Pending right-hand-side node for `lhs: rhs` syntax.
        /// Holds the first expression received after `pending_key` is set. Stored here rather
        /// than immediately committed to `bindings` so that the dot-access handler can pop it
        /// (via `pop_last_value_from_frame`) and extend it into a qualified name like `Result.Ok`.
        /// Committed to `bindings` (as an `Annotated` node) when:
        ///   - the close bracket arrives, or
        ///   - the next new binding or colon arrives (signalling the end of the RHS).
        pending_rhs: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Match arm with explicit scoping: `[case [let bindings] pattern body]`
    ///
    /// Collects exactly three positional expressions:
    ///   - `let_bindings`: the `[let ...]` node declaring which names are binding targets
    ///   - `pattern`:      the structural pattern to match against the scrutinee
    ///   - `body`:         the expression to evaluate when the arm matches
    ///
    /// Fewer than three expressions is a parse error.
    CaseDecl {
        let_bindings: Option<Arc<SurfaceNode>>,
        pattern: Option<Arc<SurfaceNode>>,
        body: Vec<Arc<SurfaceNode>>,
        span_start: Span,
    },
    /// Pipe operator: `lhs | rhs`
    /// Holds the LHS and waits for the RHS to be parsed
    Pipe {
        lhs: Arc<SurfaceNode>,
        /// Span of the `|` token itself.
        pipe_span: Span,
    },
    /// Annotation collection frame — collects one expression which becomes an annotation.
    ///
    /// Pushed when `@` or `@` (ImmediateAt) is seen. Closed by `drain_annotation_frames` when the
    /// annotation value expression has been received and the next token is not ImmediateAt.
    AnnotationCollect {
        target: AnnotationTarget,
        value: Option<Arc<SurfaceNode>>,
        span_start: Span,
    },
}

/// What an AnnotationCollect frame is collecting an annotation for.
#[derive(Debug, Clone)]
enum AnnotationTarget {
    /// Annotation attaches to a previously completed expression (`x@Type`)
    Attached(Arc<SurfaceNode>),
    /// Floating annotation — will be applied to the next expression in the parent frame (`[@Type expr]`)
    Floating,
    /// Function return annotation (`fn@Type`) — stored in parent Fn frame's return_ann field
    FnReturn,
}

// AnnotationTarget contains Arc<SurfaceNode> which doesn't implement PartialEq naturally,
// but StackFrame only requires Clone+Debug for our purposes.
impl PartialEq for AnnotationTarget {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AnnotationTarget::Floating, AnnotationTarget::Floating) => true,
            (AnnotationTarget::FnReturn, AnnotationTarget::FnReturn) => true,
            (AnnotationTarget::Attached(a), AnnotationTarget::Attached(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Intermediate representation for call arguments (positional or named).
///
/// During call parsing, arguments are collected in order as they appear in the source.
/// Named and positional arguments may be freely interleaved — the parser imposes no ordering.
#[derive(Debug, Clone, PartialEq)]
enum CallArg {
    Positional(Arc<SurfaceNode>),
    /// Named argument, with optional field annotation from `field@Child: value` syntax.
    ///
    /// The annotation is `Some(...)` when the argument name was written as `name@Ann:`,
    /// and `None` for ordinary `name:` named arguments.
    Named(String, Arc<SurfaceNode>, Option<Spanned<Annotation>>),
}

/// Parse output: AST plus comment side-channels for the formatter.
///
/// `leading_comments` are keyed by a `u64` span key encoding `(start_line << 32 | start_col)`
/// of the node they precede.
/// `trailing_comments` are keyed by the same encoding for the node they follow.
/// `blank_before` is keyed by the same encoding and set to `true` when
/// there was a blank line (consecutive newlines) before that node.
///
/// `diagnostics` contains parse diagnostics (always `kind = "parse-error"`, level = Err) that
/// were recovered from during parsing. These are errors that occurred inside bracket forms;
/// the parser substituted an `SurfaceExpression::Error` node and continued. Fatal errors
/// (lexer failure, unclosed brackets at top level) still cause `parse()` to return `Err(...)`.
///
/// Pipeline stages that do not need comment data should access `.program` directly.
/// The formatter uses all fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutput {
    pub leading_comments: BTreeMap<u64, Vec<String>>,
    pub trailing_comments: BTreeMap<u64, String>,
    pub blank_before: BTreeMap<u64, bool>,
    /// Recovered parse diagnostics (errors inside bracket forms where the parser continued).
    pub diagnostics: Vec<TypeDiagnostic>,
    /// The parsed Surface AST program — the primary output of the parser.
    pub program: crate::ast::SurfaceProgram,
}

impl ParseOutput {
    /// Get the parsed program as a `SurfaceProgram`.
    ///
    /// The parser constructs `SurfaceProgram` natively; `program` is the primary
    /// output field. This method is a convenience accessor that returns a reference
    /// to it. Prefer accessing `ParseOutput::program` directly where possible.
    pub fn as_surface_program(&self) -> &crate::ast::SurfaceProgram {
        &self.program
    }
}

/// Given a token slice and a start index pointing just past an `[`,
/// advance until the matching `]` is found (tracking nesting depth).
///
/// Returns the index of the `CloseBracket` token that closes the bracket opened before
/// `from_idx` (i.e. the `]` whose depth-count reaches zero). If no matching `]` is
/// found before the end of the token slice, returns `tokens.len()` (pointing past the end).
///
/// `from_idx` is the index of the first token *inside* the bracket (not the `[` itself).
/// The returned index points at the closing `]`, so the caller should advance past it
/// with `i = result + 1`.
fn skip_to_closing_bracket(tokens: &[Spanned<Token>], from_idx: usize) -> usize {
    let mut depth: usize = 1; // we are already inside one bracket
    let mut idx = from_idx;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::OpenBracket => depth += 1,
            Token::CloseBracket => {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    // No matching close found — return past-end sentinel
    tokens.len()
}

// Map a `StackFrame` to a human-readable context string for diagnostic notes.
impl std::fmt::Display for StackFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StackFrame::Match { .. } => write!(f, "match expression"),
            StackFrame::Dict { .. } => write!(f, "dict"),
            StackFrame::Call { .. } => write!(f, "call expression"),
            StackFrame::Fn { .. } => write!(f, "fn expression"),
            StackFrame::LetDecl { .. } => write!(f, "let binding"),
            StackFrame::TypeAlias { .. } => write!(f, "type declaration"),
            StackFrame::ClassDecl { .. } => write!(f, "class declaration"),
            StackFrame::InstanceDecl { .. } => write!(f, "instance declaration"),
            StackFrame::PatternDecl { .. } => write!(f, "pattern declaration"),
            StackFrame::Quote { .. } => write!(f, "quote expression"),
            StackFrame::Unquote { .. } => write!(f, "unquote expression"),
            StackFrame::UnquoteSplice { .. } => write!(f, "unquote-splice expression"),
            StackFrame::SyntaxClass { .. } => write!(f, "syntax class"),
            StackFrame::AnnotationCollect { .. } => write!(f, "annotation"),
            StackFrame::CaseDecl { .. } => write!(f, "case arm"),
            StackFrame::Pipe { .. } => write!(f, "pipe expression"),
        }
    }
}

/// Recover from a parse error that occurred *inside* a bracket form (the frame is already pushed).
///
/// Called when an error occurs while processing tokens inside an existing `StackFrame`. This function:
/// 1. Records the error in `diagnostics` with a context note from the innermost frame.
/// 2. Pops the innermost `StackFrame` (which contained the error).
/// 3. For Dict/Call frames: builds a partial expression with valid entries collected so far,
///    plus an `SurfaceExpression::Error` entry for the malformed part.
/// 4. For other frames: pushes `SurfaceExpression::Error(error_span)` to the parent.
/// 5. Skips `i` past the `]` that closes the abandoned frame, accounting for nested brackets.
///
/// After calling this, the caller should `continue` the main token loop.
///
/// `error_span`: the span to use for the `SurfaceExpression::Error` node.
/// `skip_from_idx`: index of the first token to search from when looking for the matching `]`.
///
/// Returns the new token index (pointing past the closing `]`, or at `tokens.len()` if not found).
fn recover_from_bracket_error(
    error: ParseError,
    error_span: Span,
    tokens: &[Spanned<Token>],
    skip_from_idx: usize,
    stack: &mut Vec<StackFrame>,
    current_document_items: &mut Vec<SurfaceItem>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) -> usize {
    let mut diag = TypeDiagnostic::error("parse-error", error.message, error_span.clone());
    if let Some(frame) = stack.last() {
        diag = diag.with_note(format!("while parsing {}", frame));
    }
    diagnostics.push(diag);

    // Pop the frame that contained the error (the innermost one).
    // If the stack is empty, we have nothing to pop — this shouldn't happen if the caller
    // checked `!stack.is_empty()`, but handle it gracefully.
    let frame = if !stack.is_empty() { stack.pop() } else { None };

    // Build a partial expression with valid entries plus an error marker.
    let partial_expr = if let Some(frame) = frame {
        match frame {
            StackFrame::Dict {
                entries,
                span_start,
                ..
            } => {
                // If there are valid entries, build a partial dict with them plus an error entry.
                // If there are no valid entries, just emit SurfaceExpression::Error.
                if entries.is_empty() {
                    mk(
                        SurfaceExpression::Error(error_span.clone()),
                        error_span.clone(),
                    )
                } else {
                    let mut partial_entries = entries
                        .into_iter()
                        .map(|e| {
                            let entry_span = if let Some(ref key) = e.node.key {
                                Span::new(
                                    key.span.start_line,
                                    key.span.start_col,
                                    e.node.value.span.end_line,
                                    e.node.value.span.end_col,
                                    Arc::clone(&key.span.file),
                                )
                            } else {
                                e.node.value.span.clone()
                            };
                            Spanned::new(e.node, entry_span)
                        })
                        .collect::<Vec<_>>();

                    // Add the error as an auto-indexed entry
                    partial_entries.push(Spanned::new(
                        SurfaceEntry {
                            key: None,
                            value: mk(
                                SurfaceExpression::Error(error_span.clone()),
                                error_span.clone(),
                            ),
                        },
                        error_span.clone(),
                    ));

                    let dict_span = Span::new(
                        span_start.start_line,
                        span_start.start_col,
                        error_span.end_line,
                        error_span.end_col,
                        Arc::clone(&error_span.file),
                    );
                    mk(SurfaceExpression::Dict(partial_entries), dict_span)
                }
            }
            StackFrame::Call {
                func: frame_func,
                implied,
                args,
                span_start,
                ..
            } => {
                // Build a partial call with valid args plus an error arg.
                let func = if let Some(ref f) = frame_func {
                    // Implied call: func already captured
                    Some(f.clone())
                } else if !args.is_empty() {
                    // Explicit call: try to extract func from args[0]
                    match &args[0] {
                        CallArg::Positional(f) => Some(Arc::clone(f)),
                        CallArg::Named(_, _, _) => None, // Invalid
                    }
                } else {
                    None
                };

                if let Some(func) = func {
                    let mut positional_args = Vec::new();
                    let mut named_args = Vec::new();

                    // For implied calls, args starts at 0. For explicit calls, skip args[0] (the func).
                    let args_iter = if frame_func.is_some() {
                        args.into_iter()
                    } else {
                        args.into_iter().skip(1).collect::<Vec<_>>().into_iter()
                    };

                    for arg in args_iter {
                        match arg {
                            CallArg::Positional(expr) => positional_args.push(expr),
                            CallArg::Named(name, expr, ann) => {
                                let expr_span = expr.span.clone();
                                named_args.push(Spanned::new(
                                    SurfaceNamedArg {
                                        name,
                                        value: expr,
                                        annotation: ann,
                                    },
                                    expr_span,
                                ));
                            }
                        }
                    }

                    let has_args_now = !positional_args.is_empty() || !named_args.is_empty();

                    // Only build a partial call if there were actual args.
                    // If it's just [f] or [call f] with an error, emit plain Error.
                    if !has_args_now {
                        mk(
                            SurfaceExpression::Error(error_span.clone()),
                            error_span.clone(),
                        )
                    } else {
                        // Add the error as a positional argument
                        positional_args.push(mk(
                            SurfaceExpression::Error(error_span.clone()),
                            error_span.clone(),
                        ));

                        let call_span = Span::new(
                            span_start.start_line,
                            span_start.start_col,
                            error_span.end_line,
                            error_span.end_col,
                            Arc::clone(&error_span.file),
                        );
                        mk(
                            SurfaceExpression::Call {
                                func,
                                args: positional_args,
                                named_args,
                                implied,
                                pipe_span: None,
                            },
                            call_span,
                        )
                    }
                } else {
                    // No function — can't build a call, use plain error
                    mk(
                        SurfaceExpression::Error(error_span.clone()),
                        error_span.clone(),
                    )
                }
            }
            _ => {
                // For other frame types (Fn, TypeAlias, Pipe, AnnotationCollect, etc.),
                // we can't meaningfully preserve partial state, so just emit Error.
                mk(
                    SurfaceExpression::Error(error_span.clone()),
                    error_span.clone(),
                )
            }
        }
    } else {
        mk(
            SurfaceExpression::Error(error_span.clone()),
            error_span.clone(),
        )
    };

    // Push the partial expression to the parent context.
    if stack.is_empty() {
        current_document_items.push(SurfaceItem::Expr(partial_expr));
    } else {
        // push_value can itself error (e.g. duplicate key during recovery).
        // Record secondary errors in diagnostics rather than discarding them.
        if let Err(secondary) = push_value(stack, current_document_items, partial_expr) {
            let secondary_span = secondary.span.unwrap_or_else(|| error_span.clone());
            diagnostics.push(TypeDiagnostic::error(
                "parse-error",
                secondary.message,
                secondary_span,
            ));
        }
    }

    // Skip past the matching `]`. `skip_to_closing_bracket` expects to be called with
    // depth=1 (already inside one bracket), starting from the first token inside the bracket.
    let close_idx = skip_to_closing_bracket(tokens, skip_from_idx);

    // Return the index past the closing `]` (or past-end if no matching close found).
    if close_idx < tokens.len() {
        close_idx + 1
    } else {
        tokens.len()
    }
}

/// Recover from a parse error that occurred when *opening* a bracket form (frame NOT yet pushed).
///
/// Called when the parser fails to push a new `StackFrame` (e.g., depth limit exceeded).
/// Since no frame was pushed, this
/// function does NOT pop anything. It:
/// 1. Records the error in `diagnostics` with a context note from the parent frame (if any).
/// 2. Pushes `SurfaceExpression::Error(error_span)` to the current top frame (or document).
/// 3. Skips `i` past the `]` that closes the bracket that failed to open.
///
/// `skip_from_idx` should be the token index just after the `[` that triggered the error
/// (i.e., the first token *inside* the bracket we failed to process).
///
/// Returns the new token index (pointing past the closing `]`, or at `tokens.len()` if not found).
fn recover_from_failed_open(
    error: ParseError,
    error_span: Span,
    tokens: &[Spanned<Token>],
    skip_from_idx: usize,
    stack: &mut Vec<StackFrame>,
    current_document_items: &mut Vec<SurfaceItem>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) -> usize {
    let mut diag = TypeDiagnostic::error("parse-error", error.message, error_span.clone());
    if let Some(frame) = stack.last() {
        diag = diag.with_note(format!("while parsing {}", frame));
    }
    diagnostics.push(diag);

    // Push SurfaceExpression::Error into the current top frame (without popping — no frame was pushed).
    let error_expr = mk(
        SurfaceExpression::Error(error_span.clone()),
        error_span.clone(),
    );

    if stack.is_empty() {
        current_document_items.push(SurfaceItem::Expr(error_expr));
    } else {
        // push_value can itself error (e.g. duplicate key during recovery).
        // Record secondary errors in diagnostics rather than discarding them.
        if let Err(secondary) = push_value(stack, current_document_items, error_expr) {
            let secondary_span = secondary.span.unwrap_or_else(|| error_span.clone());
            diagnostics.push(TypeDiagnostic::error(
                "parse-error",
                secondary.message,
                secondary_span,
            ));
        }
    }

    // Skip past the matching `]`.
    let close_idx = skip_to_closing_bracket(tokens, skip_from_idx);

    if close_idx < tokens.len() {
        close_idx + 1
    } else {
        tokens.len()
    }
}

/// Parse tinct source text using the iterative parser.
///
/// This is the main entry point for Phase 2c-1 (complete feature set). The parser handles:
/// - Basic literals: `Int`, `Float`, `StringLiteral`, `Identifier`, `EscapedRef`
/// - Dicts: `[]`, `[42]`, `[a: 1 b: 2]`, keyed and auto-indexed entries
/// - Call forms: `[call $f arg1 arg2 name: val]`
/// - Fn forms: `[fn [let x y@Int ...rest] body]`, `[fn@Type [let params...] body]` with full param parsing
/// - Type-alias: `[type expr]`
/// - Type-assert: `[@Annotation expr]`
/// - Dot access chains: `$a.b.c`, `$a.0` (identifier and integer keys)
/// - Pipe expressions: `$a | $f` (reverse-apply)
/// - Document separators: `---` between document sections
/// - Comment collection: leading and trailing comments attached by span offset
///
/// When errors occur inside bracket forms, the parser recovers by substituting an
/// `SurfaceExpression::Error` node and skipping to the matching `]`. Recovered errors are collected
/// in `ParseOutput.errors`. Fatal errors (lexer failure, unclosed brackets) still
/// cause this function to return `Err(...)`.
pub fn parse(source: &str, file: Arc<str>) -> Result<ParseOutput, TypeDiagnostic> {
    // Tokenize the input via the lexer
    let tokens = lexer::tokenize(source, Arc::clone(&file))
        .map_err(|e| TypeDiagnostic::error("parse-error", e.message, e.span))?;

    // Stack of frames tracking bracket nesting
    let mut stack: Vec<StackFrame> = Vec::new();

    // Current document being built (one or more items: expressions and declarations)
    let mut current_document_items: Vec<SurfaceItem> = Vec::new();

    // All documents in the file
    let mut documents: Vec<Spanned<Arc<SurfaceDocument>>> = Vec::new();

    // Comment maps (keyed by span_key(start_line, start_col))
    let mut leading_comments: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut trailing_comments: BTreeMap<u64, String> = BTreeMap::new();
    let mut blank_before: BTreeMap<u64, bool> = BTreeMap::new();

    // Recovered parse diagnostics (errors inside bracket forms).
    let mut diagnostics: Vec<TypeDiagnostic> = Vec::new();

    // Track the span of the last significant token for trailing comment detection
    let mut last_significant_span: Option<Span> = None;

    // Track the last frame popped by a CloseBracket — when there are unclosed brackets at EOF,
    // the last popped frame is the extra bracket that consumed the expected outer close.
    let mut last_popped_frame: Option<(&'static str, Span)> = None;

    // Phase 1: Track next document's header (parsed from --- line)
    let mut next_doc_header: indexmap::IndexMap<String, Arc<SurfaceNode>> =
        indexmap::IndexMap::new();

    // Track all section names seen across the file to detect duplicates.
    let mut seen_section_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    fn frame_info_static(frame: &StackFrame) -> (&'static str, Span) {
        match frame {
            StackFrame::Dict { span_start, .. } => ("dict", span_start.clone()),
            StackFrame::Call { span_start, .. } => ("call", span_start.clone()),
            StackFrame::Fn { span_start, .. } => ("fn", span_start.clone()),
            StackFrame::TypeAlias { span_start, .. } => ("type", span_start.clone()),
            StackFrame::Quote { span_start, .. } => ("quote", span_start.clone()),
            StackFrame::Unquote { span_start, .. } => ("unquote", span_start.clone()),
            StackFrame::UnquoteSplice { span_start, .. } => ("unquote-splice", span_start.clone()),
            StackFrame::SyntaxClass { span_start, .. } => ("syntax-class", span_start.clone()),
            StackFrame::Match { span_start, .. } => ("match", span_start.clone()),
            StackFrame::ClassDecl { span_start, .. } => ("class", span_start.clone()),
            StackFrame::InstanceDecl { span_start, .. } => ("instance", span_start.clone()),
            StackFrame::PatternDecl { span_start, .. } => ("pattern", span_start.clone()),
            StackFrame::LetDecl { span_start, .. } => ("let", span_start.clone()),
            StackFrame::CaseDecl { span_start, .. } => ("case", span_start.clone()),
            StackFrame::Pipe { pipe_span, .. } => ("pipe", pipe_span.clone()),
            StackFrame::AnnotationCollect { span_start, .. } => ("annotation", span_start.clone()),
        }
    }

    // Convert to index-based iteration for peeking
    let token_vec = tokens;
    let mut i = 0;

    while i < token_vec.len() {
        let spanned_token = &token_vec[i];
        let token = &spanned_token.node;
        let span = spanned_token.span.clone();

        match token {
            Token::OpenBracket => {
                // Check depth before pushing
                if stack.len() >= MAX_PARSE_DEPTH {
                    let err = ParseError {
                        message: format!(
                            "maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"
                        ),
                        span: Some(span.clone()),
                        help: None,
                    };
                    if !stack.is_empty() {
                        // Recovery: failed to open the bracket (no frame pushed yet).
                        // Skip the entire bracket form, push Error to current parent.
                        i = recover_from_failed_open(
                            err,
                            span,
                            &token_vec,
                            i + 1, // skip from inside the bracket we tried to open
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(TypeDiagnostic::error(
                        "parse-error",
                        format!("maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"),
                        span,
                    ));
                }

                // Context-sensitive rule: inside [let ...], nested brackets are always
                // sub-LetDecl binding-pattern groups, not expressions. This is the one
                // place in the parser where bracket classification is context-dependent
                // (unified-bindings.md §Parsing Invariant, point 2). The `let` keyword
                // is the announcement that makes this safe: a reader sees `[let [a b]: Pair]`
                // and knows `[a b]` is a binding group because `[let ...]` is in scope.
                //
                // Exception: if the nested bracket starts with `let` (i.e., `[let [let ...]]`),
                // fall through to the standard Token::Let dispatch so the inner bracket becomes
                // a proper LetDecl frame rather than a binding-pattern group. This allows
                // destructuring patterns that themselves introduce let-binding sublists.
                // Peek at next non-whitespace/non-newline token for form classification.
                // Used both for the LetDecl nesting guard and the form classifier below.
                let next_token = peek_next_significant(&token_vec, i);
                let next_is_let = matches!(next_token, Some((Token::Let, _)));
                if matches!(stack.last(), Some(StackFrame::LetDecl { .. })) && !next_is_let {
                    // Push a nested LetDecl for multi-payload destructuring.
                    // The pending_key for the outer LetDecl (set by the colon handler) will
                    // be consumed when this inner LetDecl closes and gets pushed as a node.
                    stack.push(StackFrame::LetDecl {
                        bindings: Vec::new(),
                        pending_key: None,
                        pending_rhs: None,
                        span_start: span.clone(),
                    });
                    i += 1; // Consume the OpenBracket
                    continue;
                }

                match next_token {
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "call"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Explicit call form: [call func args...]
                        // (Not a call form if the keyword is followed by colon: [call: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Call {
                            func: None, // func extracted from args[0]
                            implied: false,
                            args: Vec::new(),
                            pending_key: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "call" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "fn"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Fn form: [fn [let params...] body] or [fn@RetType [let params...] body]
                        // (Not a fn form if the keyword is followed by colon: [fn: x] is a dict.)
                        // (depth already checked above)

                        i += 1; // Consume the OpenBracket
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1; // Consume the "fn" token

                        // Push the Fn frame first (return_ann initially None).
                        // If there's an ImmediateAt after "fn", push an AnnotationCollect frame
                        // targeting FnReturn so drain_annotation_frames will store the annotation
                        // in the Fn frame's return_ann field.
                        stack.push(StackFrame::Fn {
                            params: Vec::new(),
                            body: Vec::new(),
                            return_ann: None,
                            span_start: span.clone(),
                            params_consumed: false,
                        });

                        // Check for return annotation: fn@RetType
                        if i < token_vec.len() && matches!(&token_vec[i].node, Token::ImmediateAt) {
                            stack.push(StackFrame::AnnotationCollect {
                                target: AnnotationTarget::FnReturn,
                                value: None,
                                span_start: token_vec[i].span.clone(),
                            });
                            i += 1; // Consume ImmediateAt
                        }

                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "type"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Type-alias form: [type expr] or [type [params] expr] or [type T1 T2 ...]
                        // (Not a type form if the keyword is followed by colon: [type: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::TypeAlias {
                            params: Vec::new(),
                            type_exprs: Vec::new(),
                            pending_key: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "type" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "quote"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Quote form: [quote expr]
                        // (Not a quote form if the keyword is followed by colon: [quote: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Quote {
                            expr: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "quote" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "unquote"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Unquote form: [unquote expr]
                        // (Not an unquote form if the keyword is followed by colon: [unquote: x] is a dict.)
                        stack.push(StackFrame::Unquote {
                            expr: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "unquote" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "unquote-splice"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Unquote-splice form: [unquote-splice expr]
                        // (Not an unquote-splice form if the keyword is followed by colon: [unquote-splice: x] is a dict.)
                        stack.push(StackFrame::UnquoteSplice {
                            expr: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "unquote-splice" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "syntax-class"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // SyntaxClass form: [syntax-class name pattern: [...] message: "..."]
                        // (Not a syntax-class form if the keyword is followed by colon: [syntax-class: x] is a dict.)
                        stack.push(StackFrame::SyntaxClass {
                            name: None,
                            pattern: None,
                            message: None,
                            pending_key: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "syntax-class" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "match"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Match form: [match scrutinee pattern: body ...]
                        // (Not a match form if the keyword is followed by colon: [match: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Match {
                            scrutinee: None,
                            arms: Vec::new(),
                            pending_pattern_expr: None,
                            pending_pattern: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "match" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Let, keyword_idx))
                        if !matches!(
                            peek_next_horizontal(&token_vec, keyword_idx),
                            Some((Token::Colon, _))
                        ) =>
                    {
                        // LetDecl form: [let x@Int y@Float z: default]
                        // (Not a let form if the keyword is followed by colon: [let: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::LetDecl {
                            bindings: Vec::new(),
                            pending_key: None,
                            pending_rhs: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "let" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Case, keyword_idx))
                        if !matches!(
                            peek_next_horizontal(&token_vec, keyword_idx),
                            Some((Token::Colon, _))
                        ) =>
                    {
                        // CaseArm form: [case [let bindings] pattern body]
                        // (Not a case form if the keyword is followed by colon: [case: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::CaseDecl {
                            let_bindings: None,
                            pattern: None,
                            body: vec![],
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "case" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "class"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Class declaration: [class [Name a] [structural-metadata] method: Type ...]
                        // (Not a class form if the keyword is followed by colon: [class: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::ClassDecl {
                            name: None,
                            params: Vec::new(),
                            superclasses: Vec::new(),
                            methods: Vec::new(),
                            pending_key: None,
                            structural_metadata: None,
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "class" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "instance"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Instance declaration: [instance ClassName [pattern ...]: methods ...]
                        // (Not an instance form if the keyword is followed by colon: [instance: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::InstanceDecl {
                            class_name: None,
                            arms: Vec::new(),
                            pending_arm_pattern: None,
                            pending_key: None,
                            current_arm_methods: Vec::new(),
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "instance" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "pattern"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Pattern declaration: [pattern [a@Int b@Float c@Float]]
                        // (Not a pattern form if the keyword is followed by colon: [pattern: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::PatternDecl {
                            bindings: Vec::new(),
                            span_start: span.clone(),
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "pattern" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::At, _)) | Some((Token::ImmediateAt, _)) => {
                        // Type-assert form: [@Annotation expr]
                        // Push a Dict frame; the @ token will create a floating annotation
                        // on it, and the CloseBracket handler will unwrap a single-entry
                        // TypeAssert dict into a TypeAssert node directly.
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.clone(),
                            floating_annotation: None,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(_name), identifier_idx))
                        if matches!(
                            peek_next_horizontal(&token_vec, identifier_idx),
                            Some((Token::Colon, _))
                        ) =>
                    {
                        // Priority 2: Identifier followed by horizontal colon → Dict
                        // [name: val] is a dict entry, not a call
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.clone(),
                            floating_annotation: None,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(_name), identifier_idx))
                        if matches!(
                            peek_next_horizontal(&token_vec, identifier_idx),
                            Some((Token::ImmediateAt, _))
                        ) =>
                    {
                        // Priority 2b: Identifier followed by ImmediateAt → Dict (data)
                        // [Foo@String] and [x@Number: 42] are data forms, not implied calls.
                        // Annotations attach to bare words in data position; call heads are never annotated.
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.clone(),
                            floating_annotation: None,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(name), _identifier_idx)) => {
                        // Priority 3: Identifier in head (not keyword, no horizontal colon, no ImmediateAt) → Implied call
                        // [f x y] calls f
                        // (depth already checked above)

                        // Consume the OpenBracket
                        i += 1;
                        // Skip whitespace to the identifier
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );

                        // Capture the identifier span and value
                        let func_span = token_vec[i].span.clone();
                        let func_name = name.clone();

                        // Consume the identifier token
                        i += 1;

                        // Create VarRef expr for the function
                        let func_expr = Arc::new(SurfaceNode::new(
                            SurfaceExpression::VarRef {
                                name: func_name,
                                escaped: false,
                                resolution: crate::ast::Resolution::new(),
                                call_dispatch: crate::ast::CallDispatch::new(),
                                annotation: None,
                                do_infer_placeholder: false,
                            },
                            func_span,
                        ));

                        stack.push(StackFrame::Call {
                            func: Some(func_expr),
                            implied: true,
                            args: Vec::new(),
                            pending_key: None,
                            span_start: span.clone(),
                        });
                        continue;
                    }
                    _ => {
                        // Priority 4 & 5: EscapedRef, literals, or anything else in head → Dict (data)
                        // [$f x y] is data, [1 2 3] is data, ["a" "b"] is data
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.clone(),
                            floating_annotation: None,
                        });
                        i += 1;
                        continue;
                    }
                }
            }

            Token::CloseBracket => {
                // Pop the frame and construct the AST node
                let frame = stack.pop().ok_or_else(|| {
                    TypeDiagnostic::error("parse-error", "unmatched closing bracket", span.clone())
                })?;
                // Record what was just popped — useful when unclosed brackets remain at EOF
                last_popped_frame = Some(frame_info_static(&frame));

                let span_end_line = span.end_line;
                let span_end_col = span.end_col;
                let span_file = span.file.clone();
                let dict_span = move |span_start: Span| {
                    Span::new(
                        span_start.start_line,
                        span_start.start_col,
                        span_end_line,
                        span_end_col,
                        Arc::clone(&span_file),
                    )
                };

                // Helper: recover from a CloseBracket-handler error (frame already popped).
                // Pushes SurfaceExpression::Error to the new top-of-stack or doc, records the error, and
                // falls through to the `last_significant_span`/`i += 1`/`continue` at the end.
                macro_rules! close_bracket_recover {
                    ($err:expr) => {{
                        let err: ParseError = $err;
                        let error_span = err.span.clone().unwrap_or_else(|| span.clone());
                        let mut diag =
                            TypeDiagnostic::error("parse-error", err.message, error_span.clone());
                        if let Some(frame) = stack.last() {
                            diag = diag.with_note(format!("while parsing {}", frame));
                        }
                        if let Some(help) = err.help {
                            diag.add_help(help);
                        }
                        diagnostics.push(diag);
                        let error_expr = mk(
                            SurfaceExpression::Error(error_span.clone()),
                            error_span.clone(),
                        );
                        // Push to parent context (stack has already had the frame popped).
                        if stack.is_empty() {
                            current_document_items.push(SurfaceItem::Expr(error_expr.clone()));
                        } else {
                            // push_value can itself error (e.g. duplicate key during recovery).
                            // Record secondary errors in diagnostics rather than discarding them.
                            if let Err(secondary) =
                                push_value(&mut stack, &mut current_document_items, error_expr)
                            {
                                let secondary_span =
                                    secondary.span.unwrap_or_else(|| error_span.clone());
                                diagnostics.push(TypeDiagnostic::error(
                                    "parse-error",
                                    secondary.message,
                                    secondary_span,
                                ));
                            }
                        }
                        // Fall through to advance i and continue.
                    }};
                }

                match frame {
                    StackFrame::Dict {
                        entries,
                        pending_key,
                        seen_keys: _,
                        span_start,
                        floating_annotation,
                    } => {
                        // If there's a pending key, that's an error — key without value
                        if let Some(key_expr) = pending_key {
                            close_bracket_recover!(ParseError {
                                message: "key without value: expected `:` and value".to_string(),
                                span: Some(key_expr.span.clone()),
                                help: None,
                            });
                        } else {
                            // Apply floating annotation if present.
                            // For `[@Type expr]` form: after the Dict closes, if there is exactly
                            // one auto-indexed entry AND a floating annotation, wrap that entry's
                            // value in TypeAssert and unwrap the Dict.
                            // This deferred application (at bracket close rather than at push_value)
                            // ensures that `x@Int` inside `[@Type x@Int]` is fully processed
                            // before the TypeAssert wraps it.
                            let entries = if let Some(ann) = floating_annotation {
                                if entries.len() == 1 && entries[0].node.key.is_none() {
                                    let entry_value =
                                        entries.into_iter().next().unwrap().node.value;
                                    let assert_span = Span::new(
                                        ann.span.start_line,
                                        ann.span.start_col,
                                        entry_value.span.end_line,
                                        entry_value.span.end_col,
                                        Arc::clone(&entry_value.span.file),
                                    );
                                    let type_assert_node = Arc::new(SurfaceNode::new(
                                        SurfaceExpression::TypeAssert {
                                            annotation: ann,
                                            expr: entry_value,
                                            resolved_type: crate::ast::TypeAnnotation::new(),
                                        },
                                        assert_span.clone(),
                                    ));
                                    // Return the TypeAssert directly, not wrapped in Dict
                                    if let Err(push_err) = push_value(
                                        &mut stack,
                                        &mut current_document_items,
                                        type_assert_node,
                                    ) {
                                        close_bracket_recover!(push_err);
                                    }
                                    // Drain any AnnotationCollect waiting for this TypeAssert as
                                    // its value (e.g. `x@[@Integer _]`). Without this call, the
                                    // early-continue below bypasses the drain at line ~2619, leaving
                                    // a stale AnnotationCollect on the stack that will corrupt all
                                    // subsequent `:` token handling in the enclosing frame.
                                    if let Err(drain_err) = drain_annotation_frames(
                                        &mut stack,
                                        &mut current_document_items,
                                        &token_vec,
                                        i + 1, // token after the `]`
                                    ) {
                                        if !stack.is_empty() {
                                            i = recover_from_bracket_error(
                                                drain_err,
                                                span,
                                                &token_vec,
                                                i + 1,
                                                &mut stack,
                                                &mut current_document_items,
                                                &mut diagnostics,
                                            );
                                            continue;
                                        }
                                        return Err(drain_err.into());
                                    }
                                    // Early continue — don't fall through to the rest of Dict handling
                                    last_significant_span = Some(span.clone());
                                    i += 1;
                                    continue;
                                } else if entries.is_empty() {
                                    // Floating annotation but no expression — error
                                    close_bracket_recover!(ParseError {
                                        message: "type-assert form [@Annotation expr] requires an expression".to_string(),
                                        span: Some(span.clone()),
                                        help: None,
                                    });
                                    // close_bracket_recover! falls through; entries won't be used
                                    vec![]
                                } else {
                                    // Floating annotation but multiple or keyed entries — ignore annotation
                                    // (This is an unusual case; the annotation was orphaned)
                                    entries
                                        .into_iter()
                                        .map(|e| {
                                            let entry_span = if let Some(ref key) = e.node.key {
                                                Span::new(
                                                    key.span.start_line,
                                                    key.span.start_col,
                                                    e.node.value.span.end_line,
                                                    e.node.value.span.end_col,
                                                    Arc::clone(&key.span.file),
                                                )
                                            } else {
                                                e.node.value.span.clone()
                                            };
                                            Spanned::new(e.node, entry_span)
                                        })
                                        .collect::<Vec<_>>()
                                }
                            } else {
                                entries
                                    .into_iter()
                                    .map(|e| {
                                        let entry_span = if let Some(ref key) = e.node.key {
                                            Span::new(
                                                key.span.start_line,
                                                key.span.start_col,
                                                e.node.value.span.end_line,
                                                e.node.value.span.end_col,
                                                Arc::clone(&key.span.file),
                                            )
                                        } else {
                                            e.node.value.span.clone()
                                        };
                                        Spanned::new(e.node, entry_span)
                                    })
                                    .collect::<Vec<_>>()
                            };

                            // CHANGE 15: If this Dict has exactly one auto-indexed entry that is a
                            // TypeAssert (from the `[@Type expr]` floating-annotation form), unwrap
                            // it and return the TypeAssert directly rather than wrapping in a Dict.
                            let should_unwrap_type_assert = entries.len() == 1
                                && entries[0].node.key.is_none()
                                && matches!(
                                    entries[0].node.value.expr,
                                    SurfaceExpression::TypeAssert { .. }
                                );

                            // B-295 fix: when a Dict contains exactly one auto-indexed entry
                            // and that entry is a Pipe expression, unwrap it and return the
                            // Pipe directly. This allows `[[call ...] | f | g]` to work as
                            // a grouped pipeline in dict entry contexts without creating an
                            // extra `{0: result}` wrapper.
                            //
                            // Example: `result: [[open file] | lines | collect]`
                            // Without unwrapping: `result: {0: ["line1", "line2", ...]}`
                            // With unwrapping: `result: ["line1", "line2", ...]`
                            //
                            // This only applies when:
                            // - Exactly one entry (no multiple values)
                            // - Entry has no key (auto-indexed, not `key: pipe-expr`)
                            // - Entry value is a Pipe expression
                            let should_unwrap_pipe = entries.len() == 1
                                && entries[0].node.key.is_none()
                                && matches!(
                                    entries[0].node.value.expr,
                                    SurfaceExpression::Pipe { .. }
                                );

                            if should_unwrap_type_assert {
                                // Unwrap: return the TypeAssert expression directly (not wrapped in Dict)
                                let type_assert_expr =
                                    entries.into_iter().next().unwrap().node.value;
                                if let Err(push_err) = push_value(
                                    &mut stack,
                                    &mut current_document_items,
                                    type_assert_expr,
                                ) {
                                    close_bracket_recover!(push_err);
                                }
                            } else if should_unwrap_pipe {
                                // Unwrap: return the Pipe expression directly
                                let pipe_expr = entries.into_iter().next().unwrap().node.value;
                                // Push to parent or document
                                if let Err(push_err) =
                                    push_value(&mut stack, &mut current_document_items, pipe_expr)
                                {
                                    close_bracket_recover!(push_err);
                                }
                            } else {
                                // Standard dict construction
                                // entries are already Spanned<SurfaceEntry> with correct spans
                                let spanned_dict =
                                    mk(SurfaceExpression::Dict(entries), dict_span(span_start));

                                // Push to parent or document (via push_value to handle pending_key)
                                if let Err(push_err) = push_value(
                                    &mut stack,
                                    &mut current_document_items,
                                    spanned_dict,
                                ) {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::Call {
                        func: frame_func,
                        implied,
                        args,
                        pending_key,
                        span_start,
                    } => {
                        // If there's a pending key, that's an error — named arg without value
                        if let Some((key, key_span, _)) = pending_key {
                            close_bracket_recover!(ParseError {
                                message: format!("named argument `{}` without value", key),
                                span: Some(key_span),
                                help: None,
                            });
                        } else {
                            // Determine the function expression
                            let func = if let Some(ref f) = frame_func {
                                // Implied call: func was captured from head Identifier at frame-push time
                                Ok(f.clone())
                            } else if args.is_empty() {
                                // Explicit call with no args: [call] is an error
                                Err(ParseError {
                                    message: "call form requires at least a function expression"
                                        .to_string(),
                                    span: Some(span.clone()),
                                    help: None,
                                })
                            } else {
                                // Explicit call: func is args[0]
                                match &args[0] {
                                    CallArg::Positional(expr) => Ok(expr.clone()),
                                    CallArg::Named(name, _, _) => Err(ParseError {
                                        message: format!(
                                            "call function cannot be a named argument (got `{name}:`)",
                                        ),
                                        span: Some(span.clone()),
                                        help: None,
                                    }),
                                }
                            };

                            match func {
                                Err(func_err) => {
                                    close_bracket_recover!(func_err);
                                }
                                Ok(func) => {
                                    let mut positional_args = Vec::new();
                                    let mut named_args = Vec::new();

                                    // For implied calls, args starts at 0. For explicit calls, skip args[0] (the func).
                                    let args_iter = if frame_func.is_some() {
                                        args.into_iter()
                                    } else {
                                        args.into_iter().skip(1).collect::<Vec<_>>().into_iter()
                                    };

                                    for arg in args_iter {
                                        match arg {
                                            CallArg::Positional(expr) => positional_args.push(expr),
                                            CallArg::Named(name, expr, ann) => {
                                                let expr_span = expr.span.clone();
                                                named_args.push(Spanned::new(
                                                    SurfaceNamedArg {
                                                        name,
                                                        value: expr,
                                                        annotation: ann,
                                                    },
                                                    expr_span,
                                                ));
                                            }
                                        }
                                    }

                                    let spanned_call = mk(
                                        SurfaceExpression::Call {
                                            func,
                                            args: positional_args,
                                            named_args,
                                            implied,
                                            pipe_span: None,
                                        },
                                        dict_span(span_start),
                                    );
                                    if let Err(push_err) = push_value(
                                        &mut stack,
                                        &mut current_document_items,
                                        spanned_call,
                                    ) {
                                        close_bracket_recover!(push_err);
                                    }
                                }
                            }
                        }
                    }

                    StackFrame::Fn {
                        params,
                        body,
                        return_ann,
                        span_start,
                        params_consumed: _,
                    } => {
                        if body.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "fn form requires a body expression".to_string(),
                                span: Some(span.clone()),
                                help: Some("a fn must have at least one body expression: [fn [let x] body]"),
                            });
                        }

                        // For single-expression bodies, use the expression directly.
                        // For multi-expression bodies, wrap in Sequential.
                        let body_expr = if body.len() == 1 {
                            body.into_iter().next().unwrap()
                        } else {
                            mk(
                                SurfaceExpression::Sequential(body),
                                dict_span(span_start.clone()),
                            )
                        };

                        let spanned_fn = mk(
                            SurfaceExpression::Fn {
                                return_ann,
                                params,
                                body: body_expr,
                                desugared: false,
                                resolved_captures: crate::ast::CapturesCell::new(),
                            },
                            dict_span(span_start),
                        );
                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_items, spanned_fn)
                        {
                            close_bracket_recover!(push_err);
                        }
                    }

                    StackFrame::TypeAlias {
                        params,
                        type_exprs,
                        pending_key,
                        span_start,
                    } => {
                        // If there's an unconsumed pending_key, the user wrote `Name:` without
                        // a payload value — this is a syntax error.
                        if let Some(key_node) = pending_key {
                            let key_name = match &key_node.expr {
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                _ => "Constructor".to_string(),
                            };
                            close_bracket_recover!(ParseError {
                                message: format!(
                                    "constructor `{key_name}:` requires a payload value (e.g. `{key_name}: [field: Type]`)"
                                ),
                                span: Some(span.clone()),
                                help: None,
                            });
                        }
                        if type_exprs.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "type-alias form requires at least one type expression"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else {
                            // Build the body.
                            //
                            // Single positional entry (no key) → use the entry value directly as body.
                            //   - `[type [port: Int]]` — structural alias body
                            //   - `[type Int]` — simple alias
                            //
                            // Multiple entries, or a single named entry → wrap in a Dict.
                            let body = if type_exprs.len() == 1 && type_exprs[0].node.key.is_none()
                            {
                                // Single positional entry — use the value directly as the body.
                                Arc::clone(&type_exprs.into_iter().next().unwrap().node.value)
                            } else {
                                // Multi-entry or named-entry — wrap entries in a Dict.
                                // type_exprs already contains Spanned<SurfaceEntry> with correct key info.
                                mk(
                                    SurfaceExpression::Dict(type_exprs),
                                    dict_span(span_start.clone()),
                                )
                            };
                            let decl = SurfaceDeclaration::TypeAlias { params, body };
                            let spanned_decl = Spanned::new(decl, dict_span(span_start));
                            if stack.is_empty() {
                                current_document_items.push(SurfaceItem::Decl(spanned_decl));
                            } else {
                                // Declaration appears inside an expression (e.g., dict value).
                                // Preserve the full declaration via SurfaceExpression::Decl so
                                // it remains traversable in expression position.
                                let node = mk(
                                    SurfaceExpression::Decl(Box::new(spanned_decl.node)),
                                    spanned_decl.span,
                                );
                                if let Err(push_err) =
                                    push_value(&mut stack, &mut current_document_items, node)
                                {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::Quote { expr, span_start } => match expr {
                        None => {
                            close_bracket_recover!(ParseError {
                                message: "quote form requires an expression".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        }
                        Some(expr) => {
                            let spanned_quote =
                                mk(SurfaceExpression::Quote(expr), dict_span(span_start));
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_items, spanned_quote)
                            {
                                close_bracket_recover!(push_err);
                            }
                        }
                    },

                    StackFrame::Unquote { expr, span_start } => match expr {
                        None => {
                            close_bracket_recover!(ParseError {
                                message: "unquote form requires an expression".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        }
                        Some(expr) => {
                            let spanned_unquote =
                                mk(SurfaceExpression::Unquote(expr), dict_span(span_start));
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_items, spanned_unquote)
                            {
                                close_bracket_recover!(push_err);
                            }
                        }
                    },

                    StackFrame::UnquoteSplice { expr, span_start } => match expr {
                        None => {
                            close_bracket_recover!(ParseError {
                                message: "unquote-splice form requires an expression".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        }
                        Some(expr) => {
                            let spanned_unquote_splice = mk(
                                SurfaceExpression::UnquoteSplice(expr),
                                dict_span(span_start),
                            );
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_items,
                                spanned_unquote_splice,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    },

                    StackFrame::SyntaxClass {
                        name,
                        pattern,
                        message,
                        pending_key,
                        span_start,
                    } => {
                        if pending_key.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "syntax-class: key without value".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if name.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "syntax-class form requires a name".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if pattern.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "syntax-class form requires a 'pattern:' field"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if let (Some(name), Some(pattern)) = (name, pattern) {
                            let decl = SurfaceDeclaration::SyntaxClass {
                                name,
                                pattern,
                                message,
                            };
                            let spanned_decl = Spanned::new(decl, dict_span(span_start));
                            if stack.is_empty() {
                                current_document_items.push(SurfaceItem::Decl(spanned_decl));
                            } else {
                                // Declaration appears inside an expression (e.g., dict value).
                                // Preserve the full declaration via SurfaceExpression::Decl so
                                // it remains traversable in expression position.
                                let node = mk(
                                    SurfaceExpression::Decl(Box::new(spanned_decl.node)),
                                    spanned_decl.span,
                                );
                                if let Err(push_err) =
                                    push_value(&mut stack, &mut current_document_items, node)
                                {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::Match {
                        scrutinee,
                        mut arms,
                        mut pending_pattern_expr,
                        pending_pattern,
                        span_start,
                    } => {
                        if scrutinee.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "match form requires a scrutinee expression".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if pending_pattern_expr.is_some() {
                            // A pending_pattern_expr at close-bracket time means the last
                            // expression was never followed by a colon, so it is not a pattern —
                            // it is the final body expression of the current arm.
                            let last_body_expr = pending_pattern_expr.take().unwrap();
                            if let Some(last_arm) = arms.last_mut() {
                                last_arm.body.push(last_body_expr);
                            } else {
                                close_bracket_recover!(ParseError {
                                    message:
                                        "match form has an expression with no arm to belong to"
                                            .to_string(),
                                    span: Some(span.clone()),
                                    help: None,
                                });
                            }
                        } else if pending_pattern.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "match pattern must be followed by a body expression"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if arms.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "match form requires at least one pattern: body pair"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if let Some(scrutinee) = scrutinee {
                            let spanned_match = mk(
                                SurfaceExpression::Match { scrutinee, arms },
                                dict_span(span_start),
                            );
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_items, spanned_match)
                            {
                                close_bracket_recover!(push_err);
                            }
                        }
                    }

                    StackFrame::ClassDecl {
                        name,
                        params,
                        superclasses,
                        methods,
                        pending_key,
                        structural_metadata,
                        span_start,
                    } => {
                        // Use empty string as placeholder if name not set (name is injected
                        // from the parent dict binding, e.g. `MyClass: [class ...]`).
                        if pending_key.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "class form has incomplete method (key without value)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else {
                            // Extract determines, resolver, injective, and superclasses from structural_metadata dict
                            let mut determines = Vec::new();
                            let mut resolver = None;
                            let mut resolver_injective = false;
                            let mut structural = String::new();
                            // superclasses comes from stack frame; may be extended by metadata
                            let mut superclasses = superclasses;

                            if let Some(metadata_expr) = structural_metadata {
                                if let SurfaceExpression::Dict(entries) = &metadata_expr.expr {
                                    for entry in entries {
                                        if let Some(ref key_expr) = entry.node.key {
                                            // Keys in the structural metadata dict are stored as
                                            // SurfaceExpression::StringLiteral (identifiers followed
                                            // by `:` are normalized to StringLiteral in ClassDecl
                                            // pending_key). Match both StringLiteral and VarRef forms.
                                            let key_name_opt = match &key_expr.expr {
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    Some(name.as_str())
                                                }
                                                SurfaceExpression::StringLiteral {
                                                    content,
                                                    ..
                                                } => Some(content.as_str()),
                                                _ => None,
                                            };
                                            if let Some(key_name) = key_name_opt {
                                                match key_name {
                                                    "determines" => {
                                                        // Extract list of FD declarations
                                                        if let SurfaceExpression::Dict(fd_entries) =
                                                            &entry.node.value.expr
                                                        {
                                                            for fd_entry in fd_entries {
                                                                determines.push(
                                                                    fd_entry.node.value.clone(),
                                                                );
                                                            }
                                                        }
                                                    }
                                                    "resolver" => {
                                                        resolver = Some(entry.node.value.clone());
                                                    }
                                                    "injective" => {
                                                        // Extract boolean value: integer encoding — 1 = injective, 0 = not injective.
                                                        // Rust protocol uses integer literals (Value::Int) for boolean results;
                                                        // prelude Boolean constructors are not part of the protocol.
                                                        if let SurfaceExpression::Int(n) =
                                                            &entry.node.value.expr
                                                        {
                                                            resolver_injective = *n != 0;
                                                        }
                                                    }
                                                    "superclasses" => {
                                                        // Value is an auto-indexed list of constraint applications:
                                                        // [[Equatable a] [Numeric b]]
                                                        // Each element is a Call expr: func=VarRef("ClassName"), args=[VarRef("param1"), ...]
                                                        if let SurfaceExpression::Dict(sc_entries) =
                                                            &entry.node.value.expr
                                                        {
                                                            for sc_entry in sc_entries {
                                                                if let SurfaceExpression::Call {
                                                                    func,
                                                                    args,
                                                                    ..
                                                                } = &sc_entry.node.value.expr
                                                                {
                                                                    if let SurfaceExpression::VarRef {
                                                                        name: class_name,
                                                                        ..
                                                                    } = &func.expr
                                                                    {
                                                                        let param_names: Vec<String> = args
                                                                            .iter()
                                                                            .filter_map(|arg| {
                                                                                if let SurfaceExpression::VarRef {
                                                                                    name,
                                                                                    ..
                                                                                } = &arg.expr
                                                                                {
                                                                                    Some(name.clone())
                                                                                } else {
                                                                                    None
                                                                                }
                                                                            })
                                                                            .collect();
                                                                        superclasses.push((
                                                                            class_name.clone(),
                                                                            param_names,
                                                                        ));
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    "structural" => {
                                                        // `structural: "closed-dict"` sets the structural discharge rule.
                                                        match &entry.node.value.expr {
                                                            SurfaceExpression::StringLiteral {
                                                                content,
                                                                ..
                                                            } => {
                                                                structural = content.clone();
                                                            }
                                                            SurfaceExpression::VarRef {
                                                                name,
                                                                ..
                                                            } => {
                                                                structural = name.clone();
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                    _ => {} // Ignore unknown metadata keys
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            let decl = SurfaceDeclaration::ClassDecl {
                                name: name.unwrap_or_else(String::new),
                                params,
                                superclasses,
                                methods: methods
                                    .into_iter()
                                    .map(|e| {
                                        let entry_span = if let Some(ref key) = e.node.key {
                                            Span::new(
                                                key.span.start_line,
                                                key.span.start_col,
                                                e.node.value.span.end_line,
                                                e.node.value.span.end_col,
                                                Arc::clone(&key.span.file),
                                            )
                                        } else {
                                            e.node.value.span.clone()
                                        };
                                        Spanned::new(e.node, entry_span)
                                    })
                                    .collect(),
                                determines,
                                resolver,
                                resolver_injective,
                                structural,
                            };
                            let spanned_decl = Spanned::new(decl, dict_span(span_start));
                            if stack.is_empty() {
                                current_document_items.push(SurfaceItem::Decl(spanned_decl));
                            } else {
                                // Declaration appears inside an expression (e.g., dict value).
                                // Preserve the full declaration via SurfaceExpression::Decl so
                                // it remains traversable in expression position.
                                let node = mk(
                                    SurfaceExpression::Decl(Box::new(spanned_decl.node)),
                                    spanned_decl.span,
                                );
                                if let Err(push_err) =
                                    push_value(&mut stack, &mut current_document_items, node)
                                {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::InstanceDecl {
                        class_name,
                        mut arms,
                        pending_arm_pattern,
                        pending_key,
                        current_arm_methods,
                        span_start,
                    } => {
                        if pending_key.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "instance form has incomplete method (key without value)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if pending_arm_pattern.is_some() {
                            close_bracket_recover!(ParseError {
                                message:
                                    "instance form has incomplete arm (pattern without methods)"
                                        .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        } else if let Some(class_name_node) = class_name {
                            // Finalize current arm methods by appending to the last arm
                            if !current_arm_methods.is_empty() {
                                if let Some(last_arm) = arms.last_mut() {
                                    // Append methods to the current arm
                                    last_arm.1.extend(current_arm_methods);
                                } else {
                                    close_bracket_recover!(ParseError {
                                        message:
                                            "instance form has orphaned methods without pattern"
                                                .to_string(),
                                        span: Some(span.clone()),
                                        help: None,
                                    });
                                }
                            }

                            let decl = SurfaceDeclaration::InstanceDecl {
                                class_name: class_name_node,
                                arms: arms
                                    .into_iter()
                                    .map(|(pattern, methods)| {
                                        let spanned_methods = methods
                                            .into_iter()
                                            .map(|e| {
                                                let entry_span = if let Some(ref key) = e.node.key {
                                                    Span::new(
                                                        key.span.start_line,
                                                        key.span.start_col,
                                                        e.node.value.span.end_line,
                                                        e.node.value.span.end_col,
                                                        Arc::clone(&key.span.file),
                                                    )
                                                } else {
                                                    e.node.value.span.clone()
                                                };
                                                Spanned::new(e.node, entry_span)
                                            })
                                            .collect();
                                        (pattern, spanned_methods)
                                    })
                                    .collect(),
                            };
                            let spanned_decl = Spanned::new(decl, dict_span(span_start));
                            if stack.is_empty() {
                                current_document_items.push(SurfaceItem::Decl(spanned_decl));
                            } else {
                                // Declaration appears inside an expression (e.g., dict value).
                                // Preserve the full declaration via SurfaceExpression::Decl so
                                // it remains traversable in expression position.
                                let node = mk(
                                    SurfaceExpression::Decl(Box::new(spanned_decl.node)),
                                    spanned_decl.span,
                                );
                                if let Err(push_err) =
                                    push_value(&mut stack, &mut current_document_items, node)
                                {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::PatternDecl {
                        bindings,
                        span_start,
                    } => {
                        let spanned_pattern = mk(
                            SurfaceExpression::PatternDecl { bindings },
                            dict_span(span_start),
                        );
                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_items, spanned_pattern)
                        {
                            close_bracket_recover!(push_err);
                        } else {
                            close_bracket_recover!(ParseError {
                                message: "instance form requires a class name".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            });
                        }
                    }

                    StackFrame::LetDecl {
                        mut bindings,
                        pending_key,
                        pending_rhs,
                        span_start,
                    } => {
                        // Commit any pending `key: rhs` pair that was not yet flushed.
                        // This is the normal exit path for `[let v: Result.Ok]` — the pair is
                        // only committed here, after the full RHS (including any dot-access
                        // extension like `.Ok`) has been assembled.
                        if let (Some(key_node), Some(rhs_node)) = (pending_key, pending_rhs) {
                            if let Err(push_err) =
                                commit_let_pending(key_node, rhs_node, &mut bindings)
                            {
                                close_bracket_recover!(push_err);
                            }
                        }
                        // Any `pending_key` without a `pending_rhs` (e.g. `z: ]`) is dropped
                        // as a best-effort recovery — the incomplete pair is silently discarded.
                        let spanned_let = mk(
                            SurfaceExpression::LetDecl { bindings },
                            dict_span(span_start),
                        );
                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_items, spanned_let)
                        {
                            close_bracket_recover!(push_err);
                        }
                    }

                    StackFrame::CaseDecl {
                        let_bindings,
                        pattern,
                        body,
                        span_start,
                    } => {
                        // CaseDecl requires [let bindings] pattern body+
                        if let (Some(let_bindings_val), Some(pattern_val)) = (let_bindings, pattern)
                        {
                            if body.is_empty() {
                                close_bracket_recover!(ParseError {
                                    message: "case arm requires [let bindings] pattern body"
                                        .to_string(),
                                    span: Some(dict_span(span_start)),
                                    help: None,
                                });
                            } else {
                                // Validate: [case ...] cannot appear when a pattern is pending.
                                // Check with immutable borrow before taking mutable borrow below.
                                if let Some(StackFrame::Match {
                                    pending_pattern_expr,
                                    pending_pattern,
                                    ..
                                }) = stack.last()
                                {
                                    if pending_pattern_expr.is_some() || pending_pattern.is_some() {
                                        close_bracket_recover!(ParseError {
                                            message: "[case ...] cannot appear when a pattern is pending — complete the prior arm first".to_string(),
                                            span: Some(dict_span(span_start.clone())),
                                            help: None,
                                        });
                                    }
                                }
                                // CaseArm data goes directly into SurfaceMatchArm.let_bindings and body fields.
                                // Push SurfaceMatchArm directly into the parent Match frame's arms list.
                                let parent_frame = stack.last_mut().ok_or_else(|| ParseError {
                                    message: "[case ...] must appear inside [match ...]"
                                        .to_string(),
                                    span: Some(dict_span(span_start.clone())),
                                    help: None,
                                });
                                match parent_frame {
                                    Ok(StackFrame::Match { arms, .. }) => {
                                        arms.push(SurfaceMatchArm {
                                            pattern: pattern_val,
                                            let_bindings: Some(let_bindings_val),
                                            guard: None,
                                            body,
                                            guard_matchable_binding:
                                                crate::ast::MatchableBinding::new(),
                                        });
                                    }
                                    Ok(_) => {
                                        close_bracket_recover!(ParseError {
                                            message: "[case ...] must appear inside [match ...]"
                                                .to_string(),
                                            span: Some(dict_span(span_start)),
                                            help: None,
                                        });
                                    }
                                    Err(e) => {
                                        close_bracket_recover!(e);
                                    }
                                }
                            }
                        } else {
                            close_bracket_recover!(ParseError {
                                message: "case arm requires [let bindings] pattern body"
                                    .to_string(),
                                span: Some(dict_span(span_start)),
                                help: None,
                            });
                        }
                    }

                    StackFrame::Pipe { .. } => {
                        // A Pipe frame is never opened by `[` — pipe is an infix operator
                        // between two expressions, not a bracket form. A CloseBracket here
                        // means the enclosing bracket form is being closed while a Pipe is
                        // on the stack, which indicates a malformed expression like `[x | ]`.
                        // Recover gracefully with a parse error instead of panicking.
                        close_bracket_recover!(ParseError {
                            message: "pipe operator '|' requires a right-hand expression"
                                .to_string(),
                            span: Some(span.clone()),
                            help: None,
                        });
                    }

                    StackFrame::AnnotationCollect { .. } => {
                        // AnnotationCollect is not a bracket form; a CloseBracket here means
                        // the enclosing bracket is closed while an annotation was still pending.
                        // This is always a parse error — valid code never produces this state.
                        // Return a fatal error: the parse is fundamentally broken at this point.
                        return Err(TypeDiagnostic::error(
                            "parse-error",
                            "annotation @ requires an expression before `]`",
                            span.clone(),
                        ));
                    }
                }

                // After a CloseBracket completes a frame and pushes a value, drain any
                // AnnotationCollect frame that was waiting for that value.
                // E.g. `x@[type: Number]` — after `]` closes the Dict, drain the AnnotationCollect.
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1, // token after the `]`
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }

                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Colon => {
                // Key-value separator
                let colon_err: Option<ParseError> = match stack.last_mut() {
                    Some(StackFrame::Dict {
                        ref mut pending_key,
                        ref mut entries,
                        seen_keys: _,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            // No pending key — check if the last entry is an auto-indexed expression
                            // that can be promoted to a computed key (e.g., `[[str "a" "b"]: val]`).
                            if let Some(last_entry) = entries.last() {
                                if last_entry.node.key.is_none() {
                                    // This is an auto-indexed entry; promote it to pending_key.
                                    let promoted = entries.pop().unwrap();
                                    // Unwrap one bracket layer: [[expr]: val] → use [expr] as key.
                                    // The promoted value is [[expr]] = Dict({0: expr}).
                                    // Extract the inner expression so eval_key_core materializes
                                    // the call/expression result rather than a Dict wrapper.
                                    let key_node = match &promoted.node.value.expr {
                                        SurfaceExpression::Dict(inner_entries)
                                            if inner_entries.len() == 1
                                                && inner_entries[0].node.key.is_none() =>
                                        {
                                            Arc::clone(&inner_entries[0].node.value)
                                        }
                                        _ => Arc::clone(&promoted.node.value),
                                    };
                                    *pending_key = Some(key_node);
                                    None // Success: next expression will be the value
                                } else {
                                    // Last entry has a key — cannot promote it
                                    Some(ParseError {
                                        message: "`:` without a key (expected key before `:`)"
                                            .to_string(),
                                        span: Some(span.clone()),
                                        help: None,
                                    })
                                }
                            } else {
                                // No entries at all
                                Some(ParseError {
                                    message: "`:` without a key (expected key before `:`)"
                                        .to_string(),
                                    span: Some(span.clone()),
                                    help: None,
                                })
                            }
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::Call {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a name (expected bare word before `:` for named arg)".to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::ClassDecl {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a method name (expected method: Type)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::InstanceDecl {
                        ref mut pending_arm_pattern,
                        ref mut pending_key,
                        ref mut arms,
                        ref mut current_arm_methods,
                        ..
                    }) => {
                        // InstanceDecl colon can be:
                        // 1. After a pattern expr → starts a new arm (store pattern, clear for method entries)
                        // 2. After a method name → method entry within current arm
                        if let Some(pattern_expr) = pending_arm_pattern.take() {
                            // This colon follows a pattern expression → new arm starts.
                            // Push the pattern as a new arm (methods will be collected in current_arm_methods
                            // and finalized at CloseBracket or when the next arm starts).
                            // First, finalize any previous arm's methods.
                            if !current_arm_methods.is_empty() {
                                if let Some(last_arm) = arms.last_mut() {
                                    last_arm.1.extend(std::mem::take(current_arm_methods));
                                }
                            }
                            arms.push((pattern_expr, Vec::new()));
                            None
                        } else if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a method name or pattern (expected method: impl or [pattern ...]: methods)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::Match {
                        ref mut pending_pattern_expr,
                        ref mut pending_pattern,
                        ..
                    }) => {
                        if pending_pattern_expr.is_none() {
                            Some(ParseError {
                                message: "`:` without a pattern (expected pattern: body in match)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else {
                            // Store the pending pattern expression directly as the pattern.
                            let surface_node = pending_pattern_expr.take().unwrap();
                            *pending_pattern = Some((surface_node, None));
                            None // Next expression will be the body
                        }
                    }
                    Some(StackFrame::SyntaxClass {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message:
                                    "`:` without a key (expected 'pattern' or 'message' before `:`)"
                                        .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::LetDecl {
                        ref mut pending_key,
                        ref mut bindings,
                        ..
                    }) => {
                        // Structural-test separator or named-param-with-default: `:` inside [let ...]
                        // The last binding pushed should be a VarRef (single name) or a LetDecl
                        // (multi-payload group `[a b]`). Pop it from bindings and store as pending_key.
                        // The rhs (constructor name or default value) will arrive as the next expression.
                        if let Some(last_binding) = bindings.last() {
                            match &last_binding.expr {
                                SurfaceExpression::VarRef { .. }
                                | SurfaceExpression::LetDecl { .. } => {
                                    let key_node = Arc::clone(last_binding);
                                    bindings.pop();
                                    *pending_key = Some(key_node);
                                    None // Next value will be rhs (constructor or default)
                                }
                                _ => Some(ParseError {
                                    message: "`:` in [let ...] must follow a bare identifier or a binding group `[a b]`".to_string(),
                                    span: Some(span.clone()),
                                    help: None,
                                }),
                            }
                        } else {
                            Some(ParseError {
                                message: "`:` without a left-hand side in [let ...] form"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        }
                    }
                    Some(StackFrame::TypeAlias {
                        ref mut pending_key,
                        ref mut type_exprs,
                        ..
                    }) => {
                        // Named constructor form: `File: [path: String]` inside [type ...].
                        // The last pushed type expression must be a bare uppercase VarRef (the
                        // constructor name). Pop it from type_exprs and store as pending_key.
                        if pending_key.is_some() {
                            // Already have a pending key — double colon error.
                            Some(ParseError {
                                message: "`:` without a value (expected constructor payload after `Name:`)"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        } else if let Some(last_entry) = type_exprs.last() {
                            if last_entry.node.key.is_none() {
                                match &last_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. }
                                        if name.chars().next().is_some_and(|c| c.is_uppercase()) =>
                                    {
                                        // Pop the last positional entry, use its value as the key.
                                        let popped = type_exprs.pop().unwrap();
                                        *pending_key = Some(Arc::clone(&popped.node.value));
                                        None // Next expression will be the payload value
                                    }
                                    _ => Some(ParseError {
                                        message: "`:` in [type ...] must follow an uppercase constructor name (e.g. `File: [path: String]`)".to_string(),
                                        span: Some(span.clone()),
                                        help: None,
                                    }),
                                }
                            } else {
                                Some(ParseError {
                                    message: "`:` without a constructor name in [type ...] form"
                                        .to_string(),
                                    span: Some(span.clone()),
                                    help: None,
                                })
                            }
                        } else {
                            Some(ParseError {
                                message: "`:` without a constructor name in [type ...] form"
                                    .to_string(),
                                span: Some(span.clone()),
                                help: None,
                            })
                        }
                    }
                    Some(frame) => {
                        let (form_name, open_pos) = frame_info_static(frame);
                        Some(ParseError {
                            message: format!(
                                "`:` is not valid inside a {} form (opened at {}:{})",
                                form_name, open_pos.start_line, open_pos.start_col
                            ),
                            span: Some(span.clone()),
                            help: None,
                        })
                    }
                    None => Some(ParseError {
                        message: "`:` at document top level (no enclosing bracket form)"
                            .to_string(),
                        span: Some(span.clone()),
                        help: None,
                    }),
                };
                if let Some(err) = colon_err {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(err.into());
                }
                i += 1;
                continue;
            }

            // Literals: collect as values, but detect colon-ahead for dict key position.
            Token::Int(n) => {
                let expr = mk(SurfaceExpression::Int(*n), span.clone());
                // Check if this integer is a potential dict key (e.g. [0: $x]).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr.clone());
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(push_err.into());
                }
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1,
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::U64Lit(n) => {
                let expr = mk(SurfaceExpression::U64(*n), span.clone());
                if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(push_err.into());
                }
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1,
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Float(f) => {
                let expr = mk(SurfaceExpression::Float(*f), span.clone());
                if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(push_err.into());
                }
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1,
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::StringLiteral {
                prefix,
                delimiter,
                content,
            } => {
                let expr = mk(
                    SurfaceExpression::StringLiteral {
                        prefix: prefix.clone(),
                        delimiter: delimiter.clone(),
                        content: content.clone(),
                    },
                    span.clone(),
                );
                // Check if this string literal is a potential dict key (e.g. ["key": value]).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if matches!(stack.last(), Some(StackFrame::Dict { .. }))
                    && peek_next_horizontal(&token_vec, i)
                        .map(|(t, _)| matches!(t, Token::Colon))
                        .unwrap_or(false)
                {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr);
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(push_err.into());
                }
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1,
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Identifier(s) => {
                // Identifiers are variable references in value position, string keys in key position.
                // Check if this is a potential key (next token is colon).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
                    // This identifier is a key candidate
                    match stack.last_mut() {
                        Some(StackFrame::Dict {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Dict key: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: s.clone(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Call {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Named arg key — store the string name (no annotation for plain `name:`)
                            *pending_key = Some((s.clone(), span.clone(), None));
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::ClassDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in class: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: s.clone(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::InstanceDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in instance: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: s.clone(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::SyntaxClass {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Field name in syntax-class: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: s.clone(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Match {
                            ref mut pending_pattern_expr,
                            ..
                        }) => {
                            // Pattern identifier in match: store as VarRef in pending_pattern_expr
                            let pattern_expr = Arc::new(SurfaceNode::new(
                                SurfaceExpression::VarRef {
                                    name: s.clone(),
                                    escaped: false,
                                    resolution: crate::ast::Resolution::new(),
                                    call_dispatch: crate::ast::CallDispatch::new(),
                                    annotation: None,
                                    do_infer_placeholder: false,
                                },
                                span.clone(),
                            ));
                            *pending_pattern_expr = Some(pattern_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        _ => {
                            // Not in dict/call context; treat as normal value (VarRef)
                            let expr = Arc::new(SurfaceNode::new(
                                SurfaceExpression::VarRef {
                                    name: s.clone(),
                                    escaped: false,
                                    resolution: crate::ast::Resolution::new(),
                                    call_dispatch: crate::ast::CallDispatch::new(),
                                    annotation: None,
                                    do_infer_placeholder: false,
                                },
                                span.clone(),
                            ));
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_items, expr)
                            {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        push_err,
                                        span,
                                        &token_vec,
                                        i + 1,
                                        &mut stack,
                                        &mut current_document_items,
                                        &mut diagnostics,
                                    );
                                    continue;
                                }
                                return Err(push_err.into());
                            }
                            if let Err(drain_err) = drain_annotation_frames(
                                &mut stack,
                                &mut current_document_items,
                                &token_vec,
                                i + 1,
                            ) {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        drain_err,
                                        span,
                                        &token_vec,
                                        i + 1,
                                        &mut stack,
                                        &mut current_document_items,
                                        &mut diagnostics,
                                    );
                                    continue;
                                }
                                return Err(drain_err.into());
                            }
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    // Not followed by colon; regular value (VarRef)
                    let expr = Arc::new(SurfaceNode::new(
                        SurfaceExpression::VarRef {
                            name: s.clone(),
                            escaped: false,
                            resolution: crate::ast::Resolution::new(),
                            call_dispatch: crate::ast::CallDispatch::new(),
                            annotation: None,
                            do_infer_placeholder: false,
                        },
                        span.clone(),
                    ));
                    if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr)
                    {
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                push_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        return Err(push_err.into());
                    }
                    if let Err(drain_err) = drain_annotation_frames(
                        &mut stack,
                        &mut current_document_items,
                        &token_vec,
                        i + 1,
                    ) {
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                drain_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        return Err(drain_err.into());
                    }
                    last_significant_span = Some(span);
                    i += 1;
                    continue;
                }
            }

            Token::EscapedRef(name) => {
                let expr = Arc::new(SurfaceNode::new(
                    SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped: true,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                        do_infer_placeholder: false,
                    },
                    span.clone(),
                ));
                // Check if this VarRef is a potential dict key (followed by colon).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr.clone());
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(push_err.into());
                }
                if let Err(drain_err) = drain_annotation_frames(
                    &mut stack,
                    &mut current_document_items,
                    &token_vec,
                    i + 1,
                ) {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            drain_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(drain_err.into());
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Comment(comment_text) => {
                // Determine if this is a trailing or leading comment based on line position
                // Trailing: comment on the same line as the previous significant token
                // Leading: comment on a different line (or no previous token)
                if let Some(prev_span) = last_significant_span.clone() {
                    if prev_span.start_line == span.start_line {
                        // Same line as previous token → trailing comment
                        trailing_comments.insert(
                            span_key(prev_span.start_line, prev_span.start_col),
                            comment_text.clone(),
                        );
                    } else {
                        // Different line → leading comment for next token
                        if let Some((_, next_idx)) = peek_next_significant(&token_vec, i) {
                            let next = &token_vec[next_idx].span;
                            let next_key = span_key(next.start_line, next.start_col);
                            leading_comments
                                .entry(next_key)
                                .or_default()
                                .push(comment_text.clone());
                        }
                    }
                } else {
                    // No previous token → leading comment for next token
                    if let Some((_, next_idx)) = peek_next_significant(&token_vec, i) {
                        let next = &token_vec[next_idx].span;
                        let next_key = span_key(next.start_line, next.start_col);
                        leading_comments
                            .entry(next_key)
                            .or_default()
                            .push(comment_text.clone());
                    }
                }

                i += 1;
                continue;
            }

            Token::Newline | Token::Semicolon => {
                // Whitespace/separators — ignored
                i += 1;
                continue;
            }

            Token::DocSeparator => {
                // Document separator: finalize current document and start a new one
                if !stack.is_empty() {
                    return Err(TypeDiagnostic::error(
                        "parse-error",
                        "document separator cannot appear inside bracket expressions",
                        span,
                    ));
                }

                // Finalize current document (even if empty) with previously parsed header
                let items = std::mem::take(&mut current_document_items);
                let doc_span = if items.is_empty() {
                    // Empty document: use separator position
                    span.clone()
                } else {
                    let first_span = items.first().unwrap().span();
                    let last_span = items.last().unwrap().span();
                    Span::new(
                        first_span.start_line,
                        first_span.start_col,
                        last_span.end_line,
                        last_span.end_col,
                        Arc::clone(&first_span.file),
                    )
                };
                documents.push(Spanned::new(
                    Arc::new(SurfaceDocument {
                        header: std::mem::take(&mut next_doc_header),
                        items,
                    }),
                    doc_span,
                ));

                // Parse section header (Phase 1): generic key-value pairs
                // Format: --- key: value  key2: value2
                // This header applies to the NEXT document
                i += 1;

                while i < token_vec.len() {
                    match &token_vec[i].node {
                        Token::Newline | Token::Semicolon => {
                            // End of header
                            i += 1;
                            break;
                        }
                        Token::Identifier(key_name) => {
                            let key = key_name.clone();
                            let id_span = token_vec[i].span.clone();
                            i += 1;

                            // Handle `--- %name` syntax: identifier starting with `%` followed
                            // by something other than `:` is a named section declaration.
                            // Optionally followed immediately by `@Type` to annotate the section's
                            // output type: `--- %config@Dict`.
                            // Store as header["name"] = Str("name-without-percent") and
                            // header["output-annotation"] = annotation node if present.
                            if key.starts_with('%')
                                && (i >= token_vec.len()
                                    || !matches!(&token_vec[i].node, Token::Colon))
                            {
                                let section_name = key.trim_start_matches('%').to_string();
                                if section_name.is_empty() {
                                    // Check for %@Type (bare % with immediate @)
                                    if i < token_vec.len()
                                        && matches!(&token_vec[i].node, Token::ImmediateAt)
                                    {
                                        // Consume the annotation and ignore the section name
                                        // (bare % is still an error, but a more informative one)
                                        return Err(TypeDiagnostic::error(
                                            "parse-error",
                                            "bare '%' in section header: expected a name after '%' (e.g. '--- %name' or '--- %name@Type')",
                                            id_span,
                                        ));
                                    }
                                    return Err(TypeDiagnostic::error(
                                        "parse-error",
                                        "bare '%' in section header: expected a name after '%' (e.g. '--- %name')",
                                        id_span,
                                    ));
                                }

                                // Duplicate section name detection
                                let full_name = format!("%{}", section_name);
                                if seen_section_names.contains(&section_name) {
                                    return Err(TypeDiagnostic::error(
                                        "parse-error",
                                        format!("duplicate section name '{}' in file", full_name),
                                        id_span,
                                    ));
                                }
                                seen_section_names.insert(section_name.clone());

                                let name_node = Arc::new(SurfaceNode::new(
                                    SurfaceExpression::StringLiteral {
                                        prefix: String::new(),
                                        delimiter: "\"".to_string(),
                                        content: section_name,
                                    },
                                    id_span,
                                ));
                                next_doc_header.insert("name".to_string(), name_node);

                                // Check for optional `@Type` annotation after section name
                                // (ImmediateAt: no whitespace between name and @).
                                // `--- %config@Dict` stores a TypeAssert(Dict, null) node
                                // as header["output-annotation"] for the loader to inspect.
                                if i < token_vec.len()
                                    && matches!(&token_vec[i].node, Token::ImmediateAt)
                                {
                                    let (ann, new_i) = parse_annotation_direct(
                                        &token_vec,
                                        i,
                                        &mut leading_comments,
                                        &mut blank_before,
                                    )?;
                                    let at_span = token_vec[i].span.clone();
                                    let assert_span = Span::new(
                                        at_span.start_line,
                                        at_span.start_col,
                                        ann.span.end_line,
                                        ann.span.end_col,
                                        Arc::clone(&at_span.file),
                                    );
                                    let null_node = Arc::new(SurfaceNode::new(
                                        SurfaceExpression::Dict(Vec::new()),
                                        at_span,
                                    ));
                                    let ann_node = Arc::new(SurfaceNode::new(
                                        SurfaceExpression::TypeAssert {
                                            annotation: ann,
                                            expr: null_node,
                                            resolved_type: crate::ast::TypeAnnotation::new(),
                                        },
                                        assert_span,
                                    ));
                                    next_doc_header
                                        .insert("output-annotation".to_string(), ann_node);
                                    i = new_i;
                                }

                                // Continue to next header token
                                continue;
                            }

                            // Expect colon for key: value pairs
                            if i >= token_vec.len() || !matches!(&token_vec[i].node, Token::Colon) {
                                return Err(TypeDiagnostic::error(
                                    "parse-error",
                                    format!("expected ':' after '{}' in header", key),
                                    if i < token_vec.len() {
                                        token_vec[i].span.clone()
                                    } else {
                                        token_vec[i - 1].span.clone()
                                    },
                                ));
                            }
                            i += 1;

                            // Parse value as a simple expression — bracket or literal
                            if i >= token_vec.len() {
                                return Err(TypeDiagnostic::error(
                                    "parse-error",
                                    format!("expected value after '{}:' in header", key),
                                    token_vec[i - 1].span.clone(),
                                ));
                            }

                            // Start a temporary parse by pushing the value tokens onto the stack
                            // We need to parse one complete expression and collect it
                            let value_start_i = i;
                            let mut temp_stack: Vec<StackFrame> = Vec::new();
                            let mut temp_items: Vec<SurfaceItem> = Vec::new();
                            let value_node: Arc<SurfaceNode>;

                            // Simple approach: parse until we have exactly one item in temp_items
                            // This handles brackets, literals, identifiers
                            loop {
                                if i >= token_vec.len() {
                                    return Err(TypeDiagnostic::error(
                                        "parse-error",
                                        format!("unexpected end of input while parsing header value for '{}'", key),
                                        token_vec[i - 1].span.clone(),
                                    ));
                                }

                                // Check if we should stop (end of value)
                                match &token_vec[i].node {
                                    Token::Newline | Token::Semicolon => {
                                        // End of header — don't consume, let outer loop handle
                                        break;
                                    }
                                    Token::Identifier(_)
                                        if temp_stack.is_empty() && !temp_items.is_empty() =>
                                    {
                                        // Next key — stop here
                                        break;
                                    }
                                    _ => {}
                                }

                                // Inline simple expression parsing for header values
                                match &token_vec[i].node {
                                    Token::OpenBracket => {
                                        temp_stack.push(StackFrame::Dict {
                                            entries: Vec::new(),
                                            pending_key: None,
                                            seen_keys: std::collections::HashSet::new(),
                                            span_start: token_vec[i].span.clone(),
                                            floating_annotation: None,
                                        });
                                        i += 1;
                                    }
                                    Token::CloseBracket => {
                                        if let Some(StackFrame::Dict {
                                            entries,
                                            span_start,
                                            ..
                                        }) = temp_stack.pop()
                                        {
                                            let dict_span = Span::new(
                                                span_start.start_line,
                                                span_start.start_col,
                                                token_vec[i].span.end_line,
                                                token_vec[i].span.end_col,
                                                Arc::clone(&token_vec[i].span.file),
                                            );
                                            let dict_node = Arc::new(SurfaceNode::new(
                                                SurfaceExpression::Dict(entries),
                                                dict_span,
                                            ));
                                            if temp_stack.is_empty() {
                                                temp_items.push(SurfaceItem::Expr(dict_node));
                                                i += 1;
                                                break;
                                            } else {
                                                // Push into parent frame
                                                if let Some(StackFrame::Dict {
                                                    ref mut entries,
                                                    ..
                                                }) = temp_stack.last_mut()
                                                {
                                                    let node_span = dict_node.span.clone();
                                                    entries.push(Spanned::new(
                                                        SurfaceEntry {
                                                            key: None,
                                                            value: dict_node,
                                                        },
                                                        node_span,
                                                    ));
                                                }
                                                i += 1;
                                            }
                                        } else {
                                            return Err(TypeDiagnostic::error(
                                                "parse-error",
                                                "unexpected ']' in header value",
                                                token_vec[i].span.clone(),
                                            ));
                                        }
                                    }
                                    Token::StringLiteral {
                                        prefix,
                                        delimiter,
                                        content,
                                    } => {
                                        let lit_span = token_vec[i].span.clone();
                                        let lit_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::StringLiteral {
                                                prefix: prefix.clone(),
                                                delimiter: delimiter.clone(),
                                                content: content.clone(),
                                            },
                                            lit_span.clone(),
                                        ));
                                        if temp_stack.is_empty() {
                                            temp_items.push(SurfaceItem::Expr(lit_node));
                                            i += 1;
                                            break;
                                        } else {
                                            if let Some(StackFrame::Dict {
                                                ref mut entries, ..
                                            }) = temp_stack.last_mut()
                                            {
                                                entries.push(Spanned::new(
                                                    SurfaceEntry {
                                                        key: None,
                                                        value: lit_node,
                                                    },
                                                    lit_span,
                                                ));
                                            }
                                            i += 1;
                                        }
                                    }
                                    Token::Identifier(name) => {
                                        let id_span = token_vec[i].span.clone();
                                        let id_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::VarRef {
                                                name: name.clone(),
                                                escaped: false,
                                                resolution: crate::ast::Resolution::new(),
                                                call_dispatch: crate::ast::CallDispatch::new(),
                                                annotation: None,
                                                do_infer_placeholder: false,
                                            },
                                            id_span.clone(),
                                        ));
                                        if temp_stack.is_empty() {
                                            temp_items.push(SurfaceItem::Expr(id_node));
                                            i += 1;
                                            break;
                                        } else {
                                            if let Some(StackFrame::Dict {
                                                ref mut entries, ..
                                            }) = temp_stack.last_mut()
                                            {
                                                entries.push(Spanned::new(
                                                    SurfaceEntry {
                                                        key: None,
                                                        value: id_node,
                                                    },
                                                    id_span,
                                                ));
                                            }
                                            i += 1;
                                        }
                                    }
                                    Token::Colon => {
                                        // Handle keyed entry in dict
                                        if let Some(StackFrame::Dict {
                                            ref mut entries, ..
                                        }) = temp_stack.last_mut()
                                        {
                                            if let Some(last_entry) = entries.last_mut() {
                                                if last_entry.node.key.is_none() {
                                                    // Convert last positional to keyed
                                                    let key_node = last_entry.node.value.clone();
                                                    last_entry.node.key = Some(key_node);
                                                    i += 1;
                                                    continue;
                                                }
                                            }
                                        }
                                        return Err(TypeDiagnostic::error(
                                            "parse-error",
                                            "unexpected ':' in header value",
                                            token_vec[i].span.clone(),
                                        ));
                                    }
                                    Token::Int(n) => {
                                        let lit_span = token_vec[i].span.clone();
                                        let lit_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::Int(*n),
                                            lit_span.clone(),
                                        ));
                                        if temp_stack.is_empty() {
                                            temp_items.push(SurfaceItem::Expr(lit_node));
                                            i += 1;
                                            break;
                                        } else {
                                            if let Some(StackFrame::Dict {
                                                ref mut entries, ..
                                            }) = temp_stack.last_mut()
                                            {
                                                entries.push(Spanned::new(
                                                    SurfaceEntry {
                                                        key: None,
                                                        value: lit_node,
                                                    },
                                                    lit_span,
                                                ));
                                            }
                                            i += 1;
                                        }
                                    }
                                    Token::Float(f) => {
                                        let f_val = *f;
                                        let lit_span = token_vec[i].span.clone();
                                        let lit_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::Float(f_val),
                                            lit_span.clone(),
                                        ));
                                        if temp_stack.is_empty() {
                                            temp_items.push(SurfaceItem::Expr(lit_node));
                                            i += 1;
                                            break;
                                        } else {
                                            if let Some(StackFrame::Dict {
                                                ref mut entries, ..
                                            }) = temp_stack.last_mut()
                                            {
                                                entries.push(Spanned::new(
                                                    SurfaceEntry {
                                                        key: None,
                                                        value: lit_node,
                                                    },
                                                    lit_span,
                                                ));
                                            }
                                            i += 1;
                                        }
                                    }
                                    Token::At | Token::ImmediateAt => {
                                        // Annotation in header value position: @Type or @[prop: T ...]
                                        // Parse the annotation and wrap it in a TypeAssert with a null
                                        // (empty dict) inner expression as a placeholder. This allows
                                        // `--- expects: @Dict` and `--- caps: [%nc: @NetCap]` to parse.
                                        let at_span = token_vec[i].span.clone();
                                        let (ann, new_i) = parse_annotation_direct(
                                            &token_vec,
                                            i,
                                            &mut leading_comments,
                                            &mut blank_before,
                                        )?;
                                        let assert_span = Span::new(
                                            at_span.start_line,
                                            at_span.start_col,
                                            ann.span.end_line,
                                            ann.span.end_col,
                                            Arc::clone(&at_span.file),
                                        );
                                        // Placeholder inner expression: empty dict [] (null)
                                        let null_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::Dict(Vec::new()),
                                            at_span.clone(),
                                        ));
                                        let assert_node = Arc::new(SurfaceNode::new(
                                            SurfaceExpression::TypeAssert {
                                                annotation: ann,
                                                expr: null_node,
                                                resolved_type: crate::ast::TypeAnnotation::new(),
                                            },
                                            assert_span,
                                        ));
                                        i = new_i;
                                        if temp_stack.is_empty() {
                                            temp_items.push(SurfaceItem::Expr(assert_node));
                                            break;
                                        } else {
                                            if let Some(StackFrame::Dict {
                                                ref mut entries, ..
                                            }) = temp_stack.last_mut()
                                            {
                                                let node_span = assert_node.span.clone();
                                                entries.push(Spanned::new(
                                                    SurfaceEntry {
                                                        key: None,
                                                        value: assert_node,
                                                    },
                                                    node_span,
                                                ));
                                            }
                                        }
                                    }
                                    _ => {
                                        return Err(TypeDiagnostic::error(
                                            "parse-error",
                                            format!(
                                                "unsupported token in header value: {:?}",
                                                token_vec[i].node
                                            ),
                                            token_vec[i].span.clone(),
                                        ));
                                    }
                                }
                            }

                            // Extract the single parsed value
                            if temp_items.len() != 1 {
                                return Err(TypeDiagnostic::error(
                                    "parse-error",
                                    format!("failed to parse value for header key '{}'", key),
                                    token_vec[value_start_i].span.clone(),
                                ));
                            }
                            if let SurfaceItem::Expr(node) = temp_items.into_iter().next().unwrap()
                            {
                                value_node = node;
                            } else {
                                return Err(TypeDiagnostic::error(
                                    "parse-error",
                                    format!("header value for '{}' must be an expression", key),
                                    token_vec[value_start_i].span.clone(),
                                ));
                            }

                            // Store in header
                            next_doc_header.insert(key, value_node);
                        }
                        _ => {
                            // Unexpected token in header
                            return Err(TypeDiagnostic::error(
                                "parse-error",
                                format!(
                                    "unexpected token in section header: {:?}",
                                    token_vec[i].node
                                ),
                                token_vec[i].span.clone(),
                            ));
                        }
                    }
                }

                continue;
            }

            Token::Dot => {
                // Dot access: pop the preceding expression and create Field.
                //
                // When there is no preceding expression (stack is empty at document level, or
                // the current frame has nothing to pop), this is a leading-dot reference:
                // `.name` with `expr: None`. Leading-dot skips the innermost letrec scope
                // and resolves `name` in the parent scope — useful to reference outer bindings
                // that would otherwise be shadowed by same-dict keys.
                //
                // Leading-dot with an integer (`.0`) is rejected with a parse error since there
                // is no meaningful parent-scope numeric-key lookup.
                let target: Option<Arc<SurfaceNode>> = if stack.is_empty() {
                    if current_document_items.is_empty() {
                        // Leading-dot at document level — no target
                        None
                    } else {
                        if let Some(SurfaceItem::Decl(_)) = current_document_items.last() {
                            return Err(TypeDiagnostic::error(
                                "parse-error",
                                "dot access requires a value expression, not a declaration",
                                span,
                            ));
                        }
                        match current_document_items.pop().unwrap() {
                            SurfaceItem::Expr(node) => Some(node),
                            SurfaceItem::Decl(_) => unreachable!(),
                        }
                    }
                } else {
                    // Inside a frame — try to pop the last value; Ok(None) means the frame
                    // has no base expression (leading-dot). Err propagates genuine errors.
                    pop_last_value_from_frame(&mut stack, span.clone())?
                };

                i += 1; // Consume the Dot

                // Skip whitespace
                i +=
                    skip_whitespace_tokens(&token_vec, i, &mut leading_comments, &mut blank_before);

                if i >= token_vec.len() {
                    let err = ParseError {
                        message: "expected field name after '.'".to_string(),
                        span: Some(span.clone()),
                        help: None,
                    };
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i,
                            &mut stack,
                            &mut current_document_items,
                            &mut diagnostics,
                        );
                        continue;
                    }
                    return Err(err.into());
                }

                // Next token must be an Identifier or Int for the field name.
                // Leading-dot with an integer (`.0`) is rejected: there is no parent-scope
                // numeric-key lookup.
                match &token_vec[i].node {
                    Token::Identifier(field) => {
                        let field_key = crate::ast::DotKey::Ident(field.clone());
                        let (start_line, start_col) = target
                            .as_ref()
                            .map_or((span.start_line, span.start_col), |t| {
                                (t.span.start_line, t.span.start_col)
                            });
                        let dot_access_span = Span::new(
                            start_line,
                            start_col,
                            token_vec[i].span.end_line,
                            token_vec[i].span.end_col,
                            Arc::clone(&token_vec[i].span.file),
                        );

                        let spanned_access = Arc::new(SurfaceNode::new(
                            SurfaceExpression::Field {
                                expr: target,
                                field: field_key,
                                resolution: crate::ast::Resolution::new(),
                            },
                            dot_access_span.clone(),
                        ));

                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_items, spanned_access)
                        {
                            i = recover_from_bracket_error(
                                push_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        if let Err(drain_err) = drain_annotation_frames(
                            &mut stack,
                            &mut current_document_items,
                            &token_vec,
                            i + 1,
                        ) {
                            i = recover_from_bracket_error(
                                drain_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }

                        i += 1;
                        continue;
                    }
                    Token::Int(n) => {
                        if target.is_none() {
                            // Leading-dot with integer index is not valid syntax.
                            let err = ParseError {
                                message: "leading-dot requires an identifier field name; integer index '.N' is not valid without a target expression".to_string(),
                                span: Some(token_vec[i].span.clone()),
                                help: None,
                            };
                            if !stack.is_empty() {
                                i = recover_from_bracket_error(
                                    err,
                                    span,
                                    &token_vec,
                                    i + 1,
                                    &mut stack,
                                    &mut current_document_items,
                                    &mut diagnostics,
                                );
                                continue;
                            }
                            return Err(err.into());
                        }
                        let field_key = crate::ast::DotKey::Int(*n);
                        let (start_line, start_col) = target
                            .as_ref()
                            .map_or((span.start_line, span.start_col), |t| {
                                (t.span.start_line, t.span.start_col)
                            });
                        let dot_access_span = Span::new(
                            start_line,
                            start_col,
                            token_vec[i].span.end_line,
                            token_vec[i].span.end_col,
                            Arc::clone(&token_vec[i].span.file),
                        );

                        let spanned_access = Arc::new(SurfaceNode::new(
                            SurfaceExpression::Field {
                                expr: target,
                                field: field_key,
                                resolution: crate::ast::Resolution::new(),
                            },
                            dot_access_span.clone(),
                        ));

                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_items, spanned_access)
                        {
                            i = recover_from_bracket_error(
                                push_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        if let Err(drain_err) = drain_annotation_frames(
                            &mut stack,
                            &mut current_document_items,
                            &token_vec,
                            i + 1,
                        ) {
                            i = recover_from_bracket_error(
                                drain_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }

                        i += 1;
                        continue;
                    }
                    _ => {
                        let err = ParseError {
                            message: format!(
                                "expected field name (identifier or integer) after '.', found {:?}",
                                token_vec[i].node
                            ),
                            span: Some(token_vec[i].span.clone()),
                            help: None,
                        };
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        return Err(err.into());
                    }
                }
            }

            Token::Pipe => {
                // Pipe operator: pop the preceding expression (LHS) and push a Pipe frame
                // Precedence: . > call > |
                // Inside [...], | terminates call argument accumulation
                let lhs = if stack.is_empty() {
                    if current_document_items.is_empty() {
                        return Err(TypeDiagnostic::error(
                            "parse-error",
                            "pipe operator requires a left-hand expression before '|'",
                            span,
                        ));
                    }
                    match current_document_items.pop().unwrap() {
                        SurfaceItem::Expr(node) => node,
                        SurfaceItem::Decl(_) => {
                            return Err(TypeDiagnostic::error(
                                "parse-error",
                                "pipe operator requires a value expression, not a declaration",
                                span,
                            ));
                        }
                    }
                } else {
                    // Inside a frame — pop the last value from the current frame.
                    // Ok(None) means the frame has no preceding expression — treat as an error.
                    let no_lhs_err = ParseError {
                        message: "pipe operator requires a left-hand expression before '|'"
                            .to_string(),
                        span: Some(span.clone()),
                        help: None,
                    };
                    match pop_last_value_from_frame(&mut stack, span.clone()) {
                        Ok(Some(lhs_expr)) => lhs_expr,
                        Ok(None) => {
                            i = recover_from_bracket_error(
                                no_lhs_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        Err(pop_err) => {
                            i = recover_from_bracket_error(
                                pop_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                    }
                };

                // Push a Pipe frame to wait for the RHS expression
                stack.push(StackFrame::Pipe {
                    lhs,
                    pipe_span: span.clone(),
                });

                i += 1; // Consume the Pipe token
                continue;
            }

            Token::At | Token::ImmediateAt => {
                let is_immediate = matches!(&token_vec[i].node, Token::ImmediateAt);

                if is_immediate {
                    // ImmediateAt: no whitespace before @, so this attaches to the preceding expression.
                    // Pop the last completed expression from the current frame (for x@Type).
                    // Ok(None) means no value in frame — fall back to current_document_items.
                    // Err propagates genuine structural errors (e.g., dot in invalid position).
                    let popped =
                        pop_last_value_from_frame(&mut stack, span.clone())?.or_else(|| {
                            // Stack empty: preceding expression may be in current_document_items
                            if let Some(SurfaceItem::Expr(node)) = current_document_items.last() {
                                if stack.is_empty() {
                                    let node = Arc::clone(node);
                                    current_document_items.pop();
                                    Some(node)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });
                    if let Some(popped) = popped {
                        stack.push(StackFrame::AnnotationCollect {
                            target: AnnotationTarget::Attached(popped),
                            value: None,
                            span_start: span.clone(),
                        });
                    } else {
                        // Nothing to pop — this @ is floating (e.g. [@Type expr])
                        stack.push(StackFrame::AnnotationCollect {
                            target: AnnotationTarget::Floating,
                            value: None,
                            span_start: span.clone(),
                        });
                    }
                } else {
                    // Plain At — floating annotation (e.g. @ without whitespace context,
                    // or [@ ...] form)
                    stack.push(StackFrame::AnnotationCollect {
                        target: AnnotationTarget::Floating,
                        value: None,
                        span_start: span.clone(),
                    });
                }

                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Ellipsis => {
                // `...` produces Placeholder(name, annotation) unconditionally.
                let ellipsis_span = span;
                i += 1;
                i +=
                    skip_whitespace_tokens(&token_vec, i, &mut leading_comments, &mut blank_before);

                // Consume optional name: `...name` or bare `...`
                let (placeholder_name, name_end, name_advance) =
                    if i < token_vec.len() && matches!(&token_vec[i].node, Token::Identifier(_)) {
                        if let Token::Identifier(name) = &token_vec[i].node {
                            let n = name.clone();
                            let end_span = token_vec[i].span.clone();
                            let combined = Span {
                                file: Arc::clone(&ellipsis_span.file),
                                start_line: ellipsis_span.start_line,
                                start_col: ellipsis_span.start_col,
                                end_line: end_span.end_line,
                                end_col: end_span.end_col,
                                name: Some(Arc::from(n.as_str())),
                            };
                            (Some(n), combined, 1)
                        } else {
                            unreachable!()
                        }
                    } else {
                        (None, ellipsis_span.clone(), 0)
                    };

                // Consume optional @Annotation after the name: `...name@Type`
                let (placeholder_annotation, annotation_advance) = if placeholder_name.is_some() {
                    let after_name = i + name_advance;
                    if after_name < token_vec.len()
                        && matches!(&token_vec[after_name].node, Token::ImmediateAt)
                    {
                        let (ann, next_i) = parse_annotation_direct(
                            &token_vec,
                            after_name,
                            &mut leading_comments,
                            &mut blank_before,
                        )?;
                        (Some(ann), next_i - after_name)
                    } else {
                        (None, 0)
                    }
                } else {
                    (None, 0)
                };

                let placeholder_expr = mk(
                    SurfaceExpression::Placeholder(placeholder_name, placeholder_annotation),
                    name_end.clone(),
                );
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_items, placeholder_expr)
                {
                    i = recover_from_bracket_error(
                        push_err,
                        name_end,
                        &token_vec,
                        i + name_advance + annotation_advance,
                        &mut stack,
                        &mut current_document_items,
                        &mut diagnostics,
                    );
                    continue;
                }
                last_significant_span = Some(name_end);
                i += name_advance + annotation_advance;
                continue;
            }

            Token::Let | Token::Case => {
                // Keywords `let` and `case` can appear as dict keys when followed by colon.
                // Handle them the same as Identifier tokens in value position.
                let keyword_str = if matches!(token, Token::Let) {
                    "let"
                } else {
                    "case"
                };

                // Check if this is a key (next token is colon)
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
                    // This keyword is a dict key candidate
                    match stack.last_mut() {
                        Some(StackFrame::Dict {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Dict key: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: keyword_str.to_string(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Call {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Named arg key
                            *pending_key = Some((keyword_str.to_string(), span.clone(), None));
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::ClassDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in class: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: keyword_str.to_string(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::InstanceDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in instance: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: keyword_str.to_string(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::SyntaxClass {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Field name in syntax-class: SurfaceExpression::StringLiteral
                            let key_expr = mk(
                                SurfaceExpression::StringLiteral {
                                    prefix: String::new(),
                                    delimiter: "\"".to_string(),
                                    content: keyword_str.to_string(),
                                },
                                span.clone(),
                            );
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Match {
                            ref mut pending_pattern_expr,
                            ..
                        }) => {
                            // Pattern keyword in match: store as VarRef in pending_pattern_expr
                            // (F-10: was missing, causing let/case before `:` in match to fall
                            // through to the `_ =>` VarRef push instead of setting the pattern)
                            let pattern_expr = Arc::new(SurfaceNode::new(
                                SurfaceExpression::VarRef {
                                    name: keyword_str.to_string(),
                                    escaped: false,
                                    resolution: crate::ast::Resolution::new(),
                                    call_dispatch: crate::ast::CallDispatch::new(),
                                    annotation: None,
                                    do_infer_placeholder: false,
                                },
                                span.clone(),
                            ));
                            *pending_pattern_expr = Some(pattern_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        _ => {
                            // Not in a key-accepting context; treat as VarRef
                            let expr = Arc::new(SurfaceNode::new(
                                SurfaceExpression::VarRef {
                                    name: keyword_str.to_string(),
                                    escaped: false,
                                    resolution: crate::ast::Resolution::new(),
                                    call_dispatch: crate::ast::CallDispatch::new(),
                                    annotation: None,
                                    do_infer_placeholder: false,
                                },
                                span.clone(),
                            ));
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_items, expr)
                            {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        push_err,
                                        span,
                                        &token_vec,
                                        i + 1,
                                        &mut stack,
                                        &mut current_document_items,
                                        &mut diagnostics,
                                    );
                                    continue;
                                }
                                return Err(push_err.into());
                            }
                            if let Err(drain_err) = drain_annotation_frames(
                                &mut stack,
                                &mut current_document_items,
                                &token_vec,
                                i + 1,
                            ) {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        drain_err,
                                        span,
                                        &token_vec,
                                        i + 1,
                                        &mut stack,
                                        &mut current_document_items,
                                        &mut diagnostics,
                                    );
                                    continue;
                                }
                                return Err(drain_err.into());
                            }
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    // Not followed by colon; treat as VarRef
                    let expr = Arc::new(SurfaceNode::new(
                        SurfaceExpression::VarRef {
                            name: keyword_str.to_string(),
                            escaped: false,
                            resolution: crate::ast::Resolution::new(),
                            call_dispatch: crate::ast::CallDispatch::new(),
                            annotation: None,
                            do_infer_placeholder: false,
                        },
                        span.clone(),
                    ));
                    if let Err(push_err) = push_value(&mut stack, &mut current_document_items, expr)
                    {
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                push_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        return Err(push_err.into());
                    }
                    if let Err(drain_err) = drain_annotation_frames(
                        &mut stack,
                        &mut current_document_items,
                        &token_vec,
                        i + 1,
                    ) {
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                drain_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_items,
                                &mut diagnostics,
                            );
                            continue;
                        }
                        return Err(drain_err.into());
                    }
                    last_significant_span = Some(span);
                    i += 1;
                    continue;
                }
            }
        }
    }

    // Source file from the token stream — used for all synthetic end-of-file spans.
    // tokenize() now receives the file from parse(), so all tokens carry the correct file.
    // For empty input (no tokens, no significant spans), fall back to this Rust source
    // location — honest attribution of where the fallback was constructed.
    let eof_file: Arc<str> = token_vec
        .first()
        .map(|t| Arc::clone(&t.span.file))
        .or_else(|| last_significant_span.as_ref().map(|s| Arc::clone(&s.file)))
        .unwrap_or_else(|| Arc::clone(&crate::rust_span!().file));

    // Check for unclosed brackets
    if !stack.is_empty() {
        let innermost_frame = stack.last().unwrap();
        let (_, innermost_span) = frame_info_static(innermost_frame);

        // Build list of all unclosed brackets, outermost first
        let all_locations: Vec<String> = stack
            .iter()
            .map(|f| {
                let (kind, span) = frame_info_static(f);
                format!("{}:{} ({})", span.start_line, span.start_col, kind)
            })
            .collect();

        let unclosed_span = Span::new(
            innermost_span.start_line,
            innermost_span.start_col,
            innermost_span.start_line,
            innermost_span.start_col + 1,
            Arc::clone(&innermost_span.file),
        );

        let count = stack.len();
        // When there's exactly 1 unclosed bracket, the last popped frame is the
        // "extra" bracket that consumed the outer dict's expected close — its
        // position pinpoints where the missing ] should be.
        let hint = if count == 1 {
            let mut parts = Vec::new();
            if let Some((kind, span)) = &last_popped_frame {
                parts.push(format!(
                    "extra {} opened at {}:{} consumed the expected close",
                    kind, span.start_line, span.start_col
                ));
            }
            if let Some(s) = &last_significant_span {
                parts.push(format!("last token at {}:{}", s.start_line, s.start_col));
            }
            if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join("; "))
            }
        } else {
            last_significant_span
                .as_ref()
                .map(|s| format!(" (last token at {}:{})", s.start_line, s.start_col))
                .unwrap_or_default()
        };
        let message = if matches!(innermost_frame, StackFrame::Pipe { .. }) {
            "pipe operator '|' requires a right-hand expression".to_string()
        } else if matches!(innermost_frame, StackFrame::AnnotationCollect { .. }) {
            "annotation @ requires an expression".to_string()
        } else if count == 1 {
            format!("unclosed bracket at {}{hint}", all_locations[0])
        } else {
            format!(
                "{} unclosed brackets — open at: {}{}",
                count,
                all_locations.join(", "),
                hint
            )
        };

        return Err(TypeDiagnostic::error("parse-error", message, unclosed_span)
            .with_help("add a closing ] to complete the expression"));
    }

    // Build the final file
    // If no expressions, create one empty document
    if current_document_items.is_empty() && documents.is_empty() {
        let doc = SurfaceDocument {
            header: next_doc_header,
            items: vec![],
        };
        let doc_span = Span::new(1, 1, 1, 1, Arc::clone(&eof_file));
        documents.push(Spanned::new(Arc::new(doc), doc_span));
    } else if !current_document_items.is_empty() {
        // Finalize current document
        let doc = SurfaceDocument {
            header: next_doc_header,
            items: current_document_items,
        };
        let doc_span = Span::new(1, 1, 1, 1, Arc::clone(&eof_file));
        documents.push(Spanned::new(Arc::new(doc), doc_span));
    }

    let program = SurfaceProgram { documents };

    Ok(ParseOutput {
        leading_comments,
        trailing_comments,
        blank_before,
        diagnostics,
        program,
    })
}

/// Extract a fully-qualified dot-path name from a surface expression node.
///
/// Returns `Some("A.B.C")` for a chain of Field nodes rooted at a VarRef,
/// where every field is an `Ident`. Returns `None` for any other shape.
///
/// Used to recognise qualified constructor names like `Result.Ok` in structural
/// test annotations (`[let v: Result.Ok]`), where the RHS arrives as a
/// `Field` node after the parser resolves the `.` postfix.
fn dot_path_name(node: &SurfaceNode) -> Option<String> {
    match &node.expr {
        SurfaceExpression::VarRef {
            name,
            escaped: false,
            ..
        } => Some(name.clone()),
        SurfaceExpression::Field {
            expr: Some(inner),
            field,
            ..
        } => {
            let prefix = dot_path_name(inner)?;
            match field {
                crate::ast::DotKey::Ident(ident) => Some(format!("{}.{}", prefix, ident)),
                crate::ast::DotKey::Int(_) => None, // e.g. `A.0` is not a constructor name
            }
        }
        SurfaceExpression::Field { expr: None, .. } => None, // leading-dot is not a constructor name
        _ => None,
    }
}

/// Helper: pop the last value from the current frame for postfix operator transformation.
/// This is used by dot access and other postfix operators that need to retroactively
/// transform the previously-pushed expression.
///
/// Note: For Dict frames, this pops the entire entry and returns just the value. The caller
/// must re-push the transformed value, which will create a new entry (either keyed or auto-indexed
/// depending on whether there was a pending_key).
/// Returns `Ok(Some(node))` if a value was popped, `Ok(None)` if the frame has nothing to pop
/// (legitimate "no base expression" condition), or `Err` for a genuine structural error (e.g.,
/// dot access inside a position where it is never valid).
fn pop_last_value_from_frame(
    stack: &mut [StackFrame],
    span: Span,
) -> Result<Option<Arc<SurfaceNode>>, ParseError> {
    match stack.last_mut() {
        Some(StackFrame::Dict {
            ref mut entries,
            ref mut pending_key,
            ref mut seen_keys,
            ..
        }) => {
            // Check if there's a pending key first - if so, we haven't pushed the value yet
            // and there's nothing to pop
            if pending_key.is_some() {
                return Ok(None);
            }
            if entries.is_empty() {
                return Ok(None);
            }
            let last_entry = entries.pop().unwrap();
            // Restore the key as pending_key so the transformed value will be re-associated
            // with the same key. Also remove it from seen_keys so that when push_value
            // re-inserts the completed entry it doesn't trigger a false duplicate key error.
            if let Some(ref key_expr) = last_entry.node.key {
                if let Some(key_str) = key_to_string(&key_expr.expr) {
                    seen_keys.remove(&key_str);
                }
            }
            *pending_key = last_entry.node.key;
            Ok(Some(Arc::clone(&last_entry.node.value)))
        }
        Some(StackFrame::Call {
            ref mut func,
            ref mut args,
            ref mut pending_key,
            ..
        }) => {
            if args.is_empty() {
                // No args pushed yet — try popping the function itself as the dot-access target.
                // This allows `[net.http-get ...]` where `net` is in head (func) position.
                if let Some(f) = func.take() {
                    return Ok(Some(f));
                }
                return Ok(None);
            }
            match args.pop().unwrap() {
                CallArg::Positional(expr) => Ok(Some(expr)),
                CallArg::Named(name, expr, ann) => {
                    // Restore the name and annotation as pending_key so the transformed value
                    // will be re-associated with the same name (and annotation, if present).
                    // This allows `[foo bar: baz.field]` and `[Ctor field@Ann: baz.field]`
                    // to work correctly with dot-access on the value side.
                    *pending_key = Some((name, span, ann));
                    Ok(Some(expr))
                }
            }
        }
        Some(StackFrame::Fn { ref mut body, .. }) => {
            if let Some(b) = body.pop() {
                Ok(Some(b))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::TypeAlias {
            ref mut type_exprs,
            ref mut pending_key,
            ..
        }) => {
            // Pop last type expression so @ annotation can attach to it.
            // This supports `Fn@Number` inside `[type Fn@Number]`.
            // If a pending_key is set (constructor name before `:` payload), pop that instead.
            if pending_key.is_some() {
                // The pending key is the last thing pushed; pop it so annotation can attach.
                let key = pending_key.take().unwrap();
                Ok(Some(key))
            } else if let Some(last_entry) = type_exprs.pop() {
                // Return the value of the last entry. Keyed entries should not be in
                // type_exprs yet when this is called (pending_key handles them before commit).
                Ok(Some(Arc::clone(&last_entry.node.value)))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::Quote { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(Some(e))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::Unquote { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(Some(e))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::UnquoteSplice { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(Some(e))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::Match {
            ref mut scrutinee,
            ref mut arms,
            ref mut pending_pattern_expr,
            ref mut pending_pattern,
            ..
        }) => {
            if pending_pattern_expr.is_some() {
                // Pop the pending pattern expression (before colon)
                Ok(Some(pending_pattern_expr.take().unwrap()))
            } else if pending_pattern.is_some() {
                // Last push was a pattern (already converted) — dot access on a pattern is not
                // supported; signal "nothing to pop" so the caller handles it as no base expression.
                Ok(None)
            } else if !arms.is_empty() {
                // Pop the last body expression of the last arm for dot-chaining.
                // If there is only one body expression, pop the entire arm and restore
                // its pattern+guard as pending. If there are multiple, pop just the last
                // body expression (leaving the arm with its remaining body expressions).
                let last_arm = arms.last_mut().unwrap();
                if last_arm.body.len() == 1 {
                    let arm = arms.pop().unwrap();
                    *pending_pattern = Some((arm.pattern, arm.guard));
                    Ok(Some(arm.body.into_iter().next().unwrap()))
                } else {
                    Ok(Some(last_arm.body.pop().unwrap()))
                }
            } else if let Some(s) = scrutinee.take() {
                Ok(Some(s))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::SyntaxClass { .. }) => Err(ParseError {
            message: "dot access is not valid inside syntax-class form".to_string(),
            span: Some(span),
            help: None,
        }),
        Some(StackFrame::ClassDecl { .. }) => Err(ParseError {
            message: "dot access is not valid inside class form".to_string(),
            span: Some(span),
            help: None,
        }),
        Some(StackFrame::InstanceDecl {
            ref mut class_name,
            ref pending_arm_pattern,
            ref arms,
            ..
        }) => {
            // Allow dot-access to extend the class name expression.
            // pop class_name so the dot continuation attaches to it (e.g. File.Readable).
            if class_name.is_some() && pending_arm_pattern.is_none() && arms.is_empty() {
                Ok(Some(class_name.take().unwrap()))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::PatternDecl { .. }) => Err(ParseError {
            message: "dot access is not valid inside pattern form".to_string(),
            span: Some(span),
            help: None,
        }),
        Some(StackFrame::LetDecl {
            ref mut bindings,
            ref mut pending_key,
            ref mut pending_rhs,
            ..
        }) => {
            // Dot access is valid on the RHS of a structural-test annotation (v: Type.Constructor).
            // `pending_rhs` holds the partial RHS while `pending_key` is set; pop it so the dot
            // handler can extend it into a qualified name (e.g. `Result` → `Result.Ok`).
            if pending_key.is_some() {
                if let Some(rhs) = pending_rhs.take() {
                    Ok(Some(rhs))
                } else {
                    Ok(None)
                }
            } else if let Some(last_binding) = bindings.pop() {
                // ImmediateAt annotation on a binding (e.g. `x@Type` in `[let x@Type ...]`):
                // pop the last binding so AnnotationCollect can attach the annotation to it.
                Ok(Some(last_binding))
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::CaseDecl {
            ref mut let_bindings,
            ref mut pattern,
            ref mut body,
            ..
        }) => {
            // Dot access is valid in the pattern and body positions (to form qualified names
            // like `Result.Ok` as the pattern or extend the body with field access).
            // It is not valid inside the [let bindings] position.
            if pattern.is_some() {
                // body phase: pop last body expr as dot-access target, or pattern if no body yet
                if let Some(b) = body.pop() {
                    Ok(Some(b))
                } else {
                    // pattern is set, body is not yet set — this means the pattern itself
                    // is being extended with dot access (e.g. `Result.Ok` as the pattern).
                    if let Some(p) = pattern.take() {
                        Ok(Some(p))
                    } else {
                        Ok(None)
                    }
                }
            } else if let_bindings.is_some() {
                Err(ParseError {
                    message: "dot access is not valid in case arm [let bindings] position"
                        .to_string(),
                    span: Some(span),
                    help: None,
                })
            } else {
                Ok(None)
            }
        }
        Some(StackFrame::Pipe { .. }) => Err(ParseError {
            message: "pipe operator '|' requires a right-hand expression".to_string(),
            span: Some(span),
            help: None,
        }),
        Some(StackFrame::AnnotationCollect { ref mut value, .. }) => {
            // Dot access on the annotation value (e.g. `x@A.B` — dot-extending the annotation type).
            // Pop the current value so the dot handler can extend it.
            if let Some(v) = value.take() {
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

fn push_expr_to_parent(
    stack: &mut Vec<StackFrame>,
    current_document_items: &mut Vec<SurfaceItem>,
    node: Arc<SurfaceNode>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_items.push(SurfaceItem::Expr(node));
        Ok(())
    } else {
        match stack.last_mut() {
            Some(StackFrame::Dict {
                ref mut entries, ..
            }) => {
                let node_span = node.span.clone();
                entries.push(Spanned::new(
                    SurfaceEntry {
                        key: None,
                        value: node,
                    },
                    node_span,
                ));
                Ok(())
            }
            Some(StackFrame::Call { ref mut args, .. }) => {
                args.push(CallArg::Positional(node));
                Ok(())
            }
            Some(StackFrame::Fn {
                ref mut params,
                ref mut body,
                ref mut params_consumed,
                ..
            }) => {
                // First expression: must be a [let ...] parameter list (StackFrame::LetDecl → LetDecl),
                // or an empty [] (zero-arg shorthand, equivalent to [fn [let] body]).
                // params_consumed guards against a second empty bracket being mistaken for params.
                if !*params_consumed && body.is_empty() {
                    // Allow [] as zero-arg shorthand — equivalent to [let] with no bindings.
                    if let SurfaceExpression::Dict(entries) = &node.expr {
                        if entries.is_empty() {
                            *params_consumed = true;
                            return Ok(());
                        }
                    }
                    // Only [let ...] form is accepted for non-empty param lists.
                    // [fn [x y] body] is a parse error — the param bracket must start with `let`.
                    if let SurfaceExpression::LetDecl { bindings } = &node.expr {
                        // Validate all bindings are valid parameter patterns before extracting.
                        // Valid: VarRef (bare), Annotated (typed), Rest(Some(_)) (variadic),
                        //        Placeholder (skipped wildcard).
                        // Annotated VarRef (VarRef { annotation: Some(_) }) is already
                        // covered by the VarRef { .. } arm since annotation is just a field.
                        let all_valid_params = bindings.iter().all(|binding| {
                            matches!(
                                &binding.expr,
                                SurfaceExpression::VarRef { .. }
                                    | SurfaceExpression::Placeholder(..)
                            )
                        });

                        if !all_valid_params {
                            return Err(ParseError {
                                message: "fn parameter list contains invalid binding patterns; each entry must be a name, name@Type, ...name, or _ wildcard".to_string(),
                                span: Some(node.span.clone()),
                                help: None,
                            });
                        }

                        // The parser accepts parameters in any order.

                        // Extract parameters from LetDecl bindings
                        *params_consumed = true;
                        for binding in bindings {
                            match &binding.expr {
                                SurfaceExpression::VarRef {
                                    name,
                                    annotation: Some(annotation),
                                    ..
                                } => {
                                    // Typed parameter (x@Int) — annotated VarRef
                                    params.push(Spanned::new(
                                        SurfaceParam {
                                            name: name.clone(),
                                            annotation: Some(annotation.clone()),
                                            variadic: false,
                                            resolved_annotation_type:
                                                crate::ast::TypeAnnotation::new(),
                                        },
                                        binding.span.clone(),
                                    ));
                                }
                                SurfaceExpression::VarRef {
                                    name,
                                    annotation: None,
                                    ..
                                } => {
                                    // Untyped parameter
                                    params.push(Spanned::new(
                                        SurfaceParam {
                                            name: name.clone(),
                                            annotation: None,
                                            variadic: false,
                                            resolved_annotation_type:
                                                crate::ast::TypeAnnotation::new(),
                                        },
                                        binding.span.clone(),
                                    ));
                                }
                                SurfaceExpression::Placeholder(Some(name), rest_ann) => {
                                    // Variadic parameter (...name) or ...name@Type
                                    params.push(Spanned::new(
                                        SurfaceParam {
                                            name: name.clone(),
                                            annotation: rest_ann.clone(),
                                            variadic: true,
                                            resolved_annotation_type:
                                                crate::ast::TypeAnnotation::new(),
                                        },
                                        binding.span.clone(),
                                    ));
                                }
                                SurfaceExpression::Placeholder(None, _) => {
                                    // Wildcard parameter — skip (valid but unusual)
                                    // Don't add to params, as Param requires a name
                                }
                                _ => {
                                    // Should not reach here due to all_valid_params check
                                }
                            }
                        }
                        return Ok(());
                    } else {
                        // First expression is not a [let ...] binding list — parse error.
                        // Per unified-bindings invariant: fn parameter list must use [let ...] form.
                        return Err(ParseError {
                            message: "`[fn ...]` parameter bracket must start with `let` (e.g. `[fn [let x y] body]`)"
                                .to_string(),
                            span: Some(node.span.clone()),
                            help: Some("change [fn [x y] body] to [fn [let x y] body]"),
                        });
                    }
                }
                // params already consumed — push to body
                body.push(node);
                Ok(())
            }
            Some(StackFrame::TypeAlias {
                ref mut params,
                ref mut type_exprs,
                ref mut pending_key,
                ..
            }) => {
                // If a pending_key is set, this node is the payload value for a named constructor.
                // Commit the key+value as a named entry: `File: [path: String]`
                if let Some(key_node) = pending_key.take() {
                    let entry_span = node.span.clone();
                    type_exprs.push(Spanned::new(
                        SurfaceEntry {
                            key: Some(key_node),
                            value: node,
                        },
                        entry_span,
                    ));
                    return Ok(());
                }

                // First expression: check if it's a LetDecl parameter list
                if params.is_empty() && type_exprs.is_empty() {
                    // Only LetDecl is supported for type params: [type [let a b c] ...]
                    if let SurfaceExpression::LetDecl { bindings } = &node.expr {
                        let all_lowercase_params =
                            bindings.iter().all(|binding| match &binding.expr {
                                // Both plain and annotated VarRef use the name field directly.
                                SurfaceExpression::VarRef { name, .. } => {
                                    name.chars().all(|c| c.is_lowercase() || c == '_')
                                }
                                _ => false,
                            });
                        if all_lowercase_params {
                            for binding in bindings {
                                match &binding.expr {
                                    SurfaceExpression::VarRef {
                                        name,
                                        annotation: Some(annotation),
                                        ..
                                    } => {
                                        // Annotated binding (e.g., `a@Covariant`): store name + annotation.
                                        params.push((name.clone(), Some(annotation.clone())));
                                    }
                                    SurfaceExpression::VarRef {
                                        name,
                                        annotation: None,
                                        ..
                                    } => {
                                        params.push((name.clone(), None));
                                    }
                                    _ => {}
                                }
                            }
                            return Ok(());
                        }
                    } else {
                        // Not a LetDecl. Check if it looks like a bare-name param list:
                        // [type [a b c] Body] — an implied Call whose func and all positional
                        // args are lowercase VarRefs with no named args. This is distinguishable
                        // from a real type expression like [or Int Null] (which has uppercase args).
                        // A [let ...] wrapper is required for type parameter lists.
                        let is_bare_name_param_list = if let SurfaceExpression::Call {
                            func,
                            args,
                            named_args,
                            implied: true,
                            ..
                        } = &node.expr
                        {
                            // func must be a lowercase VarRef
                            let func_is_lowercase = matches!(
                                &func.expr,
                                SurfaceExpression::VarRef { name, .. }
                                    if name.chars().all(|c| c.is_lowercase() || c == '_')
                            );
                            // all positional args must be lowercase VarRefs
                            let args_are_lowercase = args.iter().all(|arg| {
                                matches!(
                                    &arg.expr,
                                    SurfaceExpression::VarRef { name, .. }
                                        if name.chars().all(|c| c.is_lowercase() || c == '_')
                                )
                            });
                            // no named args (named args indicate a real type expression)
                            func_is_lowercase && args_are_lowercase && named_args.is_empty()
                        } else {
                            false
                        };

                        if is_bare_name_param_list {
                            return Err(ParseError {
                                message: "`[type ...]` parameter bracket must start with `let` (e.g. `[type [let a b] Body]`)"
                                    .to_string(),
                                span: Some(node.span.clone()),
                                help: Some("change [type [a b] Body] to [type [let a b] Body]"),
                            });
                        }
                    }
                }

                // Detect an implied Call with an uppercase-named func inside [type ...] body.
                // Valid type body elements are: bare VarRef (unit constructor), keyed entry
                // (named constructor via pending_key), and [let ...] type params. An implied Call
                // whose func is an uppercase VarRef is not a valid type body element.
                //
                // Detection is annotation-sensitive:
                //   - Unannotated uppercase func: ANY args (positional or named) → invalid.
                //     `[File field: Type]`      — unannotated, named args → invalid
                //     `[Ok a]`                  — unannotated, positional args → invalid
                //   - Annotated uppercase func: ONLY named args → invalid; positional args are
                //     valid as function-type alias parameters: `[Fn@Integer [Integer Integer]]`.
                //
                // Dict nodes (structural alias bodies) are not affected — they are not Calls.
                let is_invalid_call_in_type_body = match &node.expr {
                    SurfaceExpression::Call {
                        func,
                        args,
                        named_args,
                        implied: true,
                        ..
                    } => match &func.expr {
                        SurfaceExpression::VarRef {
                            name, annotation, ..
                        } if name.chars().next().is_some_and(|c| c.is_uppercase()) => {
                            if annotation.is_some() {
                                // Annotated func: only named args are invalid here; positional
                                // args are valid as function-type alias parameters,
                                // e.g. `[Fn@Integer [Integer Integer]]`.
                                !named_args.is_empty()
                            } else {
                                // Unannotated func: any args (positional or named) are invalid.
                                !args.is_empty() || !named_args.is_empty()
                            }
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if is_invalid_call_in_type_body {
                    let (ctor_name, ctor_annotation) = match &node.expr {
                        SurfaceExpression::Call { func, .. } => match &func.expr {
                            SurfaceExpression::VarRef {
                                name, annotation, ..
                            } => {
                                let ann_str = annotation.as_ref().map(|a| format!("@{}", a.node));
                                (name.clone(), ann_str)
                            }
                            _ => ("CtorName".to_string(), None),
                        },
                        _ => ("CtorName".to_string(), None),
                    };
                    let qualified = match &ctor_annotation {
                        Some(ann) => format!("{ctor_name}{ann}"),
                        None => ctor_name.clone(),
                    };
                    return Err(ParseError {
                        message: format!(
                            "unexpected call form `[{qualified} ...]` inside `[type ...]`; \
                             named constructors use keyed entry syntax: `{qualified}: [fields]`"
                        ),
                        span: Some(node.span.clone()),
                        help: None,
                    });
                }

                // T-1539: Detect multiple positional structural dict bodies.
                // A structural alias must have exactly one body (single positional dict with
                // lowercase-keyed entries). Multiple positional dicts are ambiguous and invalid.
                let is_positional_struct_dict = match &node.expr {
                    SurfaceExpression::Dict(entries) => {
                        // A dict is "structural" if it has at least one keyed entry where the key
                        // is a lowercase-starting identifier (not uppercase, not a number).
                        entries.iter().any(|e| {
                            if let Some(k) = &e.node.key {
                                matches!(&k.expr, SurfaceExpression::VarRef { name, .. }
                                    if name.chars().next().is_some_and(|c| c.is_lowercase()))
                            } else {
                                false
                            }
                        })
                    }
                    _ => false,
                };
                if is_positional_struct_dict {
                    // Check if there's already a structural dict body entry.
                    let already_has_struct_dict = type_exprs.iter().any(|e| {
                        if e.node.key.is_some() {
                            // Keyed entries are named constructors, not structural bodies.
                            return false;
                        }
                        matches!(&e.node.value.expr, SurfaceExpression::Dict(inner)
                        if inner.iter().any(|ie| {
                            if let Some(k) = &ie.node.key {
                                matches!(&k.expr, SurfaceExpression::VarRef { name, .. }
                                    if name.chars().next().is_some_and(|c| c.is_lowercase()))
                            } else {
                                false
                            }
                        }))
                    });
                    if already_has_struct_dict {
                        return Err(ParseError {
                            message: "structural alias must have exactly one body; \
                                use [or ...] for a union of structural types"
                                .to_string(),
                            span: Some(node.span.clone()),
                            help: None,
                        });
                    }
                }

                // Not a parameter list (or already have params) — this is a type expression entry.
                // Multi-entry `[type T1 T2 ...]` accumulates all positional type expressions.
                let entry_span = node.span.clone();
                type_exprs.push(Spanned::new(
                    SurfaceEntry {
                        key: None,
                        value: node,
                    },
                    entry_span,
                ));
                Ok(())
            }
            Some(StackFrame::Quote {
                expr: ref mut quote_expr,
                ..
            }) => {
                if quote_expr.is_some() {
                    return Err(ParseError {
                        message: "quote form can only have one expression".to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    });
                }
                *quote_expr = Some(node);
                Ok(())
            }
            Some(StackFrame::Unquote {
                expr: ref mut unquote_expr,
                ..
            }) => {
                if unquote_expr.is_some() {
                    return Err(ParseError {
                        message: "unquote form can only have one expression".to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    });
                }
                *unquote_expr = Some(node);
                Ok(())
            }
            Some(StackFrame::UnquoteSplice {
                expr: ref mut unquote_splice_expr,
                ..
            }) => {
                if unquote_splice_expr.is_some() {
                    return Err(ParseError {
                        message: "unquote-splice form can only have one expression".to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    });
                }
                *unquote_splice_expr = Some(node);
                Ok(())
            }
            Some(StackFrame::SyntaxClass {
                ref mut name,
                ref mut pattern,
                ref mut message,
                ref mut pending_key,
                ..
            }) => {
                // SyntaxClass expects: [syntax-class name pattern: [...] message: "..."]
                // First expression: name (VarRef)
                // Then key-value pairs for pattern and message
                if name.is_none() {
                    // First expression should be a VarRef (syntax-class name)
                    if let SurfaceExpression::VarRef { name: n, .. } = &node.expr {
                        *name = Some(n.clone());
                        Ok(())
                    } else {
                        Err(ParseError {
                            message: "syntax-class declaration requires a name (bare identifier)"
                                .to_string(),
                            span: Some(node.span.clone()),
                            help: None,
                        })
                    }
                } else if pending_key.is_some() {
                    // We have a pending key — this expression is the value
                    let key = pending_key.take().unwrap();
                    if let SurfaceExpression::VarRef { name: key_name, .. } = &key.expr {
                        match key_name.as_str() {
                            "pattern" => {
                                if pattern.is_some() {
                                    return Err(ParseError {
                                        message: "syntax-class: duplicate 'pattern' key".to_string(),
                                        span: Some(node.span.clone()),
                                        help: None,
                                    });
                                }
                                *pattern = Some(node);
                                Ok(())
                            }
                            "message" => {
                                if message.is_some() {
                                    return Err(ParseError {
                                        message: "syntax-class: duplicate 'message' key".to_string(),
                                        span: Some(node.span.clone()),
                                        help: None,
                                    });
                                }
                                if let SurfaceExpression::StringLiteral { content, .. } = &node.expr {
                                    *message = Some(content.clone());
                                    Ok(())
                                } else {
                                    Err(ParseError {
                                        message: "syntax-class 'message' value must be a string literal"
                                            .to_string(),
                                        span: Some(node.span.clone()),
                                        help: None,
                                    })
                                }
                            }
                            _ => Err(ParseError {
                                message: format!(
                                    "syntax-class: unknown key '{}' (expected 'pattern' or 'message')",
                                    key_name
                                ),
                                span: Some(key.span.clone()),
                                help: None,
                            }),
                        }
                    } else {
                        Err(ParseError {
                            message: "syntax-class keys must be bare identifiers".to_string(),
                            span: Some(key.span.clone()),
                            help: None,
                        })
                    }
                } else {
                    // No pending key — this expression should become a pending key
                    *pending_key = Some(node);
                    Ok(())
                }
            }
            Some(StackFrame::Match {
                ref mut scrutinee,
                ref mut arms,
                ref mut pending_pattern_expr,
                ref mut pending_pattern,
                ..
            }) => {
                // Match expects: [match scrutinee pattern: body ...]
                // First expression is the scrutinee
                // Then bracket expressions or identifiers followed by `:` are patterns
                // Expressions after `:` are bodies
                if scrutinee.is_none() {
                    // This is the scrutinee
                    *scrutinee = Some(node);
                    Ok(())
                } else if pending_pattern.is_none() && pending_pattern_expr.is_none() {
                    // No pending pattern or pattern expression — this is either the first
                    // pattern (will be converted on colon) or a body continuation for the
                    // last arm (if a completed arm exists and no colon follows).
                    // We can't tell yet which it is, so store it as pending_pattern_expr.
                    // The colon handler will convert it to a pattern; if no colon comes and
                    // a new expression arrives, the else branch below handles it.
                    *pending_pattern_expr = Some(node);
                    Ok(())
                } else if pending_pattern.is_some() {
                    // We have a pending pattern (raw SurfaceNode) — this must be the body (first expr)
                    let (pattern, guard) = pending_pattern.take().unwrap();
                    arms.push(SurfaceMatchArm {
                        pattern,
                        let_bindings: None,
                        guard,
                        body: vec![node],
                        guard_matchable_binding: crate::ast::MatchableBinding::new(),
                    });
                    Ok(())
                } else {
                    // pending_pattern_expr is set but not converted yet (no colon followed it).
                    // This means the old pending_pattern_expr was NOT a pattern — it is a body
                    // continuation for the current arm. Append it to the last arm's body,
                    // then store the new node as the new pending_pattern_expr.
                    let old_pending = pending_pattern_expr.take().unwrap();
                    if let Some(last_arm) = arms.last_mut() {
                        last_arm.body.push(old_pending);
                    } else {
                        return Err(ParseError {
                            message: "unexpected expression before first match arm (no pattern: body pair yet)".to_string(),
                            span: Some(old_pending.span.clone()),
                            help: None,
                        });
                    }
                    *pending_pattern_expr = Some(node);
                    Ok(())
                }
            }
            Some(StackFrame::ClassDecl {
                ref mut name,
                ref mut params,
                ref mut structural_metadata,
                superclasses: _,
                ..
            }) => {
                // ClassDecl expects: [class [let a b...] [structural-metadata] method: Type ...]
                // First expression is the class header: [let a b ...] (ALL are type params; name comes from binding)
                // Second expression (if Dict) is structural metadata: [determines: [...] resolver: ... superclasses: ...]
                if name.is_none() {
                    // This is the class header — must be a LetDecl; all bindings are type params
                    match &node.expr {
                        SurfaceExpression::LetDecl { bindings } => {
                            // LetDecl form: [class [let a b c] ...]
                            // ALL bindings are type params; class name comes from the binding position (Change B)
                            for binding in bindings.iter() {
                                if let SurfaceExpression::VarRef {
                                    name: param_name, ..
                                } = &binding.expr
                                {
                                    params.push(param_name.clone());
                                }
                                // Non-identifier params silently skipped.
                            }
                            // Mark header as parsed (name stays None until set by parent Dict frame)
                            *name = Some(String::new());
                            Ok(())
                        }
                        _ => {
                            // Not a LetDecl — parse error
                            Err(ParseError {
                                message: "class declaration requires [let ...] form (e.g. [class [let a] ...]); class name comes from the binding (e.g. MyClass: [class [let a] ...])".to_string(),
                                span: Some(node.span.clone()),
                                help: Some("change [class [a] ...] to [class [let a] ...]"),
                            })
                        }
                    }
                } else if structural_metadata.is_none() {
                    // Second positional expression after header
                    // If it's a Dict, it's structural metadata
                    // Otherwise, it should be handled by push_value (method entries)
                    match &node.expr {
                        SurfaceExpression::Dict(_) => {
                            *structural_metadata = Some(node);
                            Ok(())
                        }
                        _ => Err(ParseError {
                            message:
                                "unexpected expression in class form (expected method: Type entries or structural metadata dict)"
                                    .to_string(),
                            span: Some(node.span.clone()),
                            help: None,
                        }),
                    }
                } else {
                    // Already have name/params/structural_metadata — subsequent expressions should be handled
                    // by push_value (which handles pending_key for method entries)
                    Err(ParseError {
                        message:
                            "unexpected expression in class form (expected method: Type entries)"
                                .to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    })
                }
            }
            Some(StackFrame::InstanceDecl {
                ref mut class_name,
                ref mut pending_arm_pattern,
                ..
            }) => {
                // InstanceDecl: [instance ClassExpr [pattern [...]]: methods ...]
                // First expression is the class name — any expression, dot-access included.
                // Subsequent expressions are arm patterns.
                if class_name.is_none() {
                    // Class name: accept any expression — dot-access chains like File.Readable
                    // are valid. Semantic validation (is this actually a class?) is the
                    // resolver's job, not the parser's.
                    *class_name = Some(node);
                    Ok(())
                } else if pending_arm_pattern.is_none() {
                    // This could be a pattern expr (PatternDecl or other expr before colon)
                    // Store it and wait for colon
                    *pending_arm_pattern = Some(node);
                    Ok(())
                } else {
                    // Already have a pending pattern — shouldn't happen
                    // (colon should clear pending_arm_pattern before next expr)
                    Err(ParseError {
                        message:
                            "unexpected expression in instance form (expected colon after pattern)"
                                .to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    })
                }
            }
            Some(StackFrame::PatternDecl {
                ref mut bindings, ..
            }) => {
                // PatternDecl collects binding expressions (typically Annotated nodes).
                // The inner bracket [a@Int b@Float] is parsed as a Dict with auto-indexed
                // entries and stored as a single Dict binding — this preserves the bracket
                // structure so Display shows [pattern [a@Int b@Float]] naturally.
                bindings.push(node);
                Ok(())
            }
            Some(StackFrame::LetDecl {
                ref mut bindings,
                ref mut pending_key,
                ref mut pending_rhs,
                ..
            }) => {
                // LetDecl collects binding expressions.
                // Each element can be: VarRef (bare binding), Annotated (typed binding),
                // Rest (`...` prefixed), or named-with-default (name: default_value).
                //
                // `pending_rhs` defers commitment of `pending_key: rhs` pairs until the RHS is
                // fully assembled. This allows `Result.Ok` (which arrives as VarRef "Result"
                // followed by a dot-access step building `Field(Result, Ok)`) to be
                // committed as a single qualified name rather than prematurely as just "Result".
                if let Some(key_node) = pending_key.take() {
                    // A pending_key exists: this incoming node is the RHS (or would be the start
                    // of a new binding if pending_rhs is also set — the latter case is handled
                    // by the else branch below after committing the previous pair).
                    if let Some(prev_rhs) = pending_rhs.take() {
                        // `pending_rhs` is already set, meaning the previous `key: rhs` pair was
                        // not yet committed (the RHS was still being assembled via dot-access).
                        // The arrival of a new expression signals the previous RHS is complete.
                        // Commit it, then handle the incoming node as a new binding.
                        commit_let_pending(key_node, prev_rhs, bindings)?;
                        // Incoming node starts a new binding (no pending_key now).
                        bindings.push(node);
                    } else {
                        // First token of the RHS — store it in pending_rhs rather than committing
                        // immediately. The dot-access handler may pop it and extend it into a
                        // qualified name (e.g. `Result` → `Result.Ok`).
                        *pending_key = Some(key_node); // Restore: not consumed yet
                        *pending_rhs = Some(node);
                    }
                } else if let Some(prev_rhs) = pending_rhs.take() {
                    // No pending_key but pending_rhs is set — this shouldn't happen in a valid
                    // parse path (pending_rhs is only set while pending_key is also set).
                    // Drop the orphan rhs and treat the incoming node as a new binding.
                    let _ = prev_rhs;
                    bindings.push(node);
                } else {
                    bindings.push(node);
                }
                Ok(())
            }
            Some(StackFrame::CaseDecl {
                ref mut let_bindings,
                ref mut pattern,
                ref mut body,
                ..
            }) => {
                // CaseDecl collects [let bindings] pattern body+
                // Multiple body expressions are wrapped in Sequential (same as fn).
                if let_bindings.is_none() {
                    *let_bindings = Some(node);
                    Ok(())
                } else if pattern.is_none() {
                    *pattern = Some(node);
                    Ok(())
                } else {
                    body.push(node);
                    Ok(())
                }
            }
            Some(StackFrame::Pipe {
                lhs,
                pipe_span: op_span,
            }) => {
                // We have the RHS expression; pop the frame and create the Pipe node
                let lhs_expr = lhs.clone();
                let op_span = op_span.clone();
                let node_span = Span::new(
                    op_span.start_line,
                    op_span.start_col,
                    node.span.end_line,
                    node.span.end_col,
                    Arc::clone(&node.span.file),
                );
                stack.pop(); // Remove the Pipe frame

                let spanned_pipe = mk(
                    SurfaceExpression::Pipe {
                        lhs: lhs_expr,
                        rhs: node,
                        pipe_span: Some(op_span),
                    },
                    node_span,
                );

                // Push to parent context
                push_value(stack, current_document_items, spanned_pipe)
            }
            Some(StackFrame::AnnotationCollect { value, .. }) => {
                // Store the annotation value expression; drain_annotation_frames will close the frame.
                if value.is_some() {
                    return Err(ParseError {
                        message: "annotation accepts only one expression".to_string(),
                        span: Some(node.span.clone()),
                        help: None,
                    });
                }
                *value = Some(node);
                Ok(())
            }
            None => unreachable!("stack.is_empty() was false but last_mut returned None"),
        }
    }
}

/// Commit a `pending_key: pending_rhs` pair in a LetDecl frame into `bindings`.
///
/// Called from two sites:
///   1. `push_value` for LetDecl — when a new expression arrives while `pending_key` and
///      `pending_rhs` are both set, signalling that the RHS is complete (the next binding
///      or a new `:` has started).
///   2. The close-bracket handler — flush any remaining pending pair before building the
///      `LetDecl` node.
///
/// Returns `Ok(())` on success or a `ParseError` for invalid LHS/RHS combinations.
fn commit_let_pending(
    key_node: Arc<SurfaceNode>,
    rhs_node: Arc<SurfaceNode>,
    bindings: &mut Vec<Arc<SurfaceNode>>,
) -> Result<(), ParseError> {
    let combined_span = Span {
        file: Arc::clone(&key_node.span.file),
        start_line: key_node.span.start_line,
        start_col: key_node.span.start_col,
        end_line: rhs_node.span.end_line,
        end_col: rhs_node.span.end_col,
        name: None,
    };

    // Detect if RHS is an uppercase constructor name (bare or qualified).
    // In a [let ...] binding, a named entry whose RHS is an uppercase name is not a valid binding.
    let is_constructor_rhs =
        dot_path_name(&rhs_node).is_some_and(|s| s.starts_with(|c: char| c.is_uppercase()));

    match key_node.expr.clone() {
        // `name: Constructor` inside [let ...] — RHS is a constructor, not a valid default value.
        SurfaceExpression::VarRef { name: key_name, .. } if is_constructor_rhs => Err(ParseError {
            message: format!(
                "unexpected constructor `{}` as value in `[let ...]` binding `{}: ...`; \
                 constructor names are not valid binding defaults",
                dot_path_name(&rhs_node).unwrap_or_default(),
                key_name
            ),
            span: Some(combined_span),
            help: None,
        }),

        // `[a b]: Constructor` inside [let ...] — binding group with constructor RHS.
        SurfaceExpression::LetDecl { .. } if is_constructor_rhs => Err(ParseError {
            message: "unexpected `[...]: Constructor` form in `[let ...]` binding; \
                      constructor names are not valid binding defaults"
                .to_string(),
            span: Some(rhs_node.span.clone()),
            help: None,
        }),

        // Case 3: `name: default_value` — named param with default.
        SurfaceExpression::VarRef { name: key_name, .. } => {
            let key_span = key_node.span.clone();
            let surf_key = mk(
                SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: "default".to_string(),
                },
                key_span,
            );
            let ann = Spanned::new(
                Annotation::PropertyDict(vec![Spanned::new(
                    SurfaceEntry {
                        key: Some(surf_key),
                        value: Arc::clone(&rhs_node),
                    },
                    rhs_node.span.clone(),
                )]),
                combined_span.clone(),
            );
            let annotated = mk(
                SurfaceExpression::VarRef {
                    name: key_name,
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: Some(ann),
                    do_infer_placeholder: false,
                },
                combined_span,
            );
            bindings.push(annotated);
            Ok(())
        }

        // `[a b]: default_value` — binding group before colon with a non-constructor RHS;
        // a binding group cannot have a default value.
        SurfaceExpression::LetDecl { .. } => Err(ParseError {
            message: "unexpected `[...]` binding group before `:` in `[let ...]`; \
                      binding groups cannot have default values"
                .to_string(),
            span: Some(rhs_node.span.clone()),
            help: None,
        }),

        // Other LHS forms — should not arise from valid parse paths.
        _ => Err(ParseError {
            message: "unexpected form before `:` in [let ...] binding".to_string(),
            span: Some(key_node.span.clone()),
            help: None,
        }),
    }
}

/// Helper: if `node` contains a `SurfaceExpression::Decl(ClassDecl { name: "", ... })`, return a new
/// node with the class name set from the binding key. Otherwise return `node` unchanged.
///
/// This implements the superclass-syntax sprint Change A: class names come from the
/// binding position in an enclosing dict, not from the `[let ...]` header.
fn inject_class_name_from_key(node: &Arc<SurfaceNode>, key: &Arc<SurfaceNode>) -> Arc<SurfaceNode> {
    // Extract the key name string from the key expression.
    // Keys may be Str (bare identifiers in dict position), VarRef, or Annotated (e.g. MyClass@[doc: "..."]).
    // Both plain and annotated VarRef use the name field.
    let key_name = match &key.expr {
        SurfaceExpression::StringLiteral { content, .. } => content.clone(),
        SurfaceExpression::VarRef { name, .. } => name.clone(),
        _ => return Arc::clone(node),
    };

    // Check if the value is a ClassDecl with an empty name.
    if let SurfaceExpression::Decl(decl_box) = &node.expr {
        if let SurfaceDeclaration::ClassDecl { name, .. } = decl_box.as_ref() {
            if name.is_empty() {
                // Reconstruct with the binding name injected.
                if let SurfaceDeclaration::ClassDecl {
                    params,
                    superclasses,
                    methods,
                    determines,
                    resolver,
                    resolver_injective,
                    structural,
                    ..
                } = decl_box.as_ref().clone()
                {
                    let new_decl = SurfaceDeclaration::ClassDecl {
                        name: key_name,
                        params,
                        superclasses,
                        methods,
                        determines,
                        resolver,
                        resolver_injective,
                        structural,
                    };
                    return mk(
                        SurfaceExpression::Decl(Box::new(new_decl)),
                        node.span.clone(),
                    );
                }
            }
        }
    }
    Arc::clone(node)
}

/// Convert a surface expression to an `Annotation` value.
///
/// Used by `drain_annotation_frames` to build an `Annotation` from the collected expression.
fn expression_to_annotation(node: &SurfaceNode) -> Annotation {
    match &node.expr {
        SurfaceExpression::VarRef {
            name,
            annotation: None,
            ..
        } => {
            // `@Expr` is the quoting sentinel — convert to Annotation::Quote immediately
            // so the quoting mechanism is independent of the prelude's Expr type name.
            if name == "Expr" {
                Annotation::Quote
            } else {
                Annotation::Simple(name.clone())
            }
        }
        SurfaceExpression::VarRef {
            name,
            annotation: Some(ann),
            ..
        } => Annotation::Annotated(
            Box::new(Annotation::Simple(name.clone())),
            Box::new(ann.node.clone()),
        ),
        SurfaceExpression::Dict(entries) => Annotation::PropertyDict(entries.clone()),
        // Implied call with VarRef head (e.g. @[Seq Int] parsed as implied call) →
        // convert to PropertyDict: func as key-less entry 0, then each positional arg.
        SurfaceExpression::Call {
            implied: true,
            func,
            args,
            ..
        } if matches!(&func.expr, SurfaceExpression::VarRef { .. }) => {
            let mut entries: Vec<Spanned<SurfaceEntry>> = Vec::new();
            entries.push(Spanned::new(
                SurfaceEntry {
                    key: None,
                    value: Arc::clone(func),
                },
                func.span.clone(),
            ));
            for arg in args {
                entries.push(Spanned::new(
                    SurfaceEntry {
                        key: None,
                        value: Arc::clone(arg),
                    },
                    arg.span.clone(),
                ));
            }
            Annotation::PropertyDict(entries)
        }
        _ => {
            // Fallback: wrap in single-entry PropertyDict
            let entry = Spanned::new(
                SurfaceEntry {
                    key: None,
                    value: Arc::new(node.clone()),
                },
                node.span.clone(),
            );
            Annotation::PropertyDict(vec![entry])
        }
    }
}

/// Create an annotated AST node by attaching an annotation to a target expression.
///
/// For `VarRef` targets with no existing annotation: stores annotation in the VarRef's annotation field.
/// For all other targets (or already-annotated VarRef): wraps in `TypeAssert`.
fn create_annotated_node(
    target: Arc<SurfaceNode>,
    annotation: Spanned<Annotation>,
) -> Arc<SurfaceNode> {
    match &target.expr {
        SurfaceExpression::VarRef {
            name,
            escaped,
            resolution,
            call_dispatch,
            annotation: None,
            ..
        } => {
            let full_span = Span {
                file: Arc::clone(&target.span.file),
                start_line: target.span.start_line,
                start_col: target.span.start_col,
                end_line: annotation.span.end_line,
                end_col: annotation.span.end_col,
                name: Some(Arc::from(name.as_str())),
            };
            Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: name.clone(),
                    escaped: *escaped,
                    resolution: resolution.clone(),
                    call_dispatch: call_dispatch.clone(),
                    annotation: Some(annotation),
                    do_infer_placeholder: false,
                },
                full_span,
            ))
        }
        _ => {
            // Non-VarRef or already-annotated VarRef: wrap in TypeAssert
            let full_span = Span {
                file: Arc::clone(&target.span.file),
                start_line: target.span.start_line,
                start_col: target.span.start_col,
                end_line: annotation.span.end_line,
                end_col: annotation.span.end_col,
                name: None,
            };
            Arc::new(SurfaceNode::new(
                SurfaceExpression::TypeAssert {
                    annotation,
                    expr: target,
                    resolved_type: crate::ast::TypeAnnotation::new(),
                },
                full_span,
            ))
        }
    }
}

/// Store a floating annotation on the current top Dict frame.
///
/// Returns `Err` with a diagnostic if the top frame is not a Dict — floating annotations
/// (`[@Type expr]`) are only meaningful inside dict contexts. In other frame types (Call,
/// Fn, Pipe, etc.) the annotation would be silently discarded, which is almost certainly
/// a user mistake.
fn set_floating_annotation(
    stack: &mut [StackFrame],
    ann: Spanned<Annotation>,
) -> Result<(), ParseError> {
    if let Some(StackFrame::Dict {
        ref mut floating_annotation,
        ..
    }) = stack.last_mut()
    {
        *floating_annotation = Some(ann);
        Ok(())
    } else {
        Err(ParseError {
            message: "floating annotation not valid in this context; `[@Type expr]` is only supported inside a dict".to_string(),
            span: Some(ann.span),
            help: None,
        })
    }
}

/// Drain completed `AnnotationCollect` frames from the top of the stack.
///
/// After every value push, call this to check if the top frame is a completed
/// `AnnotationCollect`. If the NEXT token is `ImmediateAt`, leave it open (chaining).
/// Otherwise, close it: convert value to Annotation, wrap target, push to parent.
fn drain_annotation_frames(
    stack: &mut Vec<StackFrame>,
    current_document_items: &mut Vec<SurfaceItem>,
    token_vec: &[Spanned<Token>],
    i: usize,
) -> Result<(), ParseError> {
    loop {
        let is_complete = matches!(
            stack.last(),
            Some(StackFrame::AnnotationCollect { value: Some(_), .. })
        );
        if !is_complete {
            break;
        }

        // Check if next significant token is ImmediateAt (chaining: x@A@B)
        let next_is_chain = i < token_vec.len() && matches!(&token_vec[i].node, Token::ImmediateAt);
        if next_is_chain {
            break;
        }

        let frame = stack.pop().unwrap();
        if let StackFrame::AnnotationCollect {
            target,
            value: Some(ann_expr),
            span_start,
        } = frame
        {
            let annotation = expression_to_annotation(&ann_expr);
            let ann_span = Span::new(
                span_start.start_line,
                span_start.start_col,
                ann_expr.span.end_line,
                ann_expr.span.end_col,
                Arc::clone(&ann_expr.span.file),
            );
            let spanned_ann = Spanned::new(annotation, ann_span);

            match target {
                AnnotationTarget::Attached(pending_expr) => {
                    let annotated = create_annotated_node(pending_expr, spanned_ann);
                    push_value(stack, current_document_items, annotated)?;
                }
                AnnotationTarget::Floating => {
                    // Store on parent frame as floating_annotation; propagate error if not a Dict.
                    set_floating_annotation(stack, spanned_ann)?;
                }
                AnnotationTarget::FnReturn => {
                    if let Some(StackFrame::Fn { return_ann, .. }) = stack.last_mut() {
                        *return_ann = Some(spanned_ann);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Helper: push a value expression, handling keyed entries in dict/call contexts.
fn push_value(
    stack: &mut Vec<StackFrame>,
    current_document_items: &mut Vec<SurfaceItem>,
    node: Arc<SurfaceNode>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_items.push(SurfaceItem::Expr(node));
        return Ok(());
    }

    match stack.last_mut() {
        Some(StackFrame::Dict {
            ref mut entries,
            ref mut pending_key,
            ref mut seen_keys,
            ..
        }) => {
            if let Some(key) = pending_key.take() {
                // Normalize bare identifier keys to StringLiteral at parse time.
                // VarRef { escaped: false } is a bare word (e.g. `foo:`) and normalizes to Str.
                // VarRef { escaped: true } is `$foo:` — a computed key — and passes through.
                let key = match &key.expr {
                    // Normalize bare identifier keys (no annotation, not escaped) to Str.
                    // Annotated keys (`myFunc@[doc: "..."]`) are NOT normalized — they retain
                    // their VarRef form so that the annotation data is preserved for doc extraction.
                    SurfaceExpression::VarRef {
                        name,
                        escaped: false,
                        annotation: None,
                        ..
                    } => mk(
                        SurfaceExpression::StringLiteral {
                            prefix: String::new(),
                            delimiter: "\"".to_string(),
                            content: name.clone(),
                        },
                        key.span.clone(),
                    ),
                    _ => key,
                };
                // Check for duplicate key (literal keys only)
                if let Some(key_str) = key_to_string(&key.expr) {
                    if seen_keys.contains(&key_str) {
                        return Err(ParseError {
                            message: format!("duplicate key \"{}\"", key_str),
                            span: Some(key.span.clone()),
                            help: None,
                        });
                    }
                    seen_keys.insert(key_str);
                }
                // If the value is a ClassDecl with empty name, inject the binding key as the class name.
                // This implements the "class name from binding position" invariant:
                //   `MyClass: [class [let a] ...]` → ClassDecl { name: "MyClass", params: ["a"] }
                let node = inject_class_name_from_key(&node, &key);
                // This value completes a keyed entry
                let entry_span = crate::ast::Span {
                    file: Arc::clone(&key.span.file),
                    start_line: key.span.start_line,
                    start_col: key.span.start_col,
                    end_line: node.span.end_line,
                    end_col: node.span.end_col,
                    name: key_to_string(&key.expr).map(|s| Arc::from(s.as_str())),
                };
                entries.push(Spanned::new(
                    SurfaceEntry {
                        key: Some(key),
                        value: node,
                    },
                    entry_span,
                ));
            } else {
                // Auto-indexed entry
                let node_span = node.span.clone();
                entries.push(Spanned::new(
                    SurfaceEntry {
                        key: None,
                        value: node,
                    },
                    node_span,
                ));
            }
            Ok(())
        }
        Some(StackFrame::Call {
            ref mut func,
            ref mut args,
            ref mut pending_key,
            ..
        }) => {
            if let Some((name, _, ann)) = pending_key.take() {
                // This value completes a named argument (ann is Some for `field@Ann: val` syntax)
                args.push(CallArg::Named(name, node, ann));
            } else if func.is_none() && args.is_empty() {
                // The function expression is being set/restored (e.g., after dot-access
                // on the function position: `[net.http-get ...]` pops `net` from func,
                // constructs `net.http-get`, then pushes it back as the new func).
                *func = Some(node);
            } else {
                // Positional argument
                args.push(CallArg::Positional(node));
            }
            Ok(())
        }
        Some(StackFrame::ClassDecl {
            ref name,
            ref mut methods,
            ref mut pending_key,
            ref structural_metadata,
            ..
        }) => {
            if name.is_none() {
                // Header not yet parsed — delegate to push_expr_to_parent
                push_expr_to_parent(stack, current_document_items, node)
            } else if structural_metadata.is_none() && pending_key.is_none() {
                // Header parsed but no structural metadata yet — delegate to push_expr_to_parent
                // which handles the second positional (structural metadata dict)
                push_expr_to_parent(stack, current_document_items, node)
            } else if let Some(key) = pending_key.take() {
                // This value completes a method signature entry
                let entry_span = crate::ast::Span {
                    file: Arc::clone(&key.span.file),
                    start_line: key.span.start_line,
                    start_col: key.span.start_col,
                    end_line: node.span.end_line,
                    end_col: node.span.end_col,
                    name: None,
                };
                methods.push(Spanned::new(
                    SurfaceEntry {
                        key: Some(key),
                        value: node.clone(),
                    },
                    entry_span,
                ));
                Ok(())
            } else {
                // Unexpected expression in class body — delegate to parent.
                // Semantic validation is the resolver's job, not the parser's.
                push_expr_to_parent(stack, current_document_items, node)
            }
        }
        Some(StackFrame::InstanceDecl {
            ref class_name,
            ref mut current_arm_methods,
            ref arms,
            ref mut pending_key,
            ref pending_arm_pattern,
            ..
        }) => {
            if class_name.is_none() {
                // Class name not yet parsed — delegate to push_expr_to_parent
                push_expr_to_parent(stack, current_document_items, node)
            } else if let Some(key) = pending_key.take() {
                // This value completes a method implementation entry within current arm
                let entry_span = crate::ast::Span {
                    file: Arc::clone(&key.span.file),
                    start_line: key.span.start_line,
                    start_col: key.span.start_col,
                    end_line: node.span.end_line,
                    end_col: node.span.end_col,
                    name: None,
                };
                current_arm_methods.push(Spanned::new(
                    SurfaceEntry {
                        key: Some(key),
                        value: node.clone(),
                    },
                    entry_span,
                ));
                Ok(())
            } else if pending_arm_pattern.is_none() && !arms.is_empty() {
                // An arm was set up (via colon), and a methods dict `[method: impl ...]` arrived.
                // Expand the dict entries directly into current_arm_methods so they are collected
                // per-arm. This handles the prelude's instance syntax where methods are in a
                // separate bracket: `[instance Cls [pattern]: [method1: impl1  method2: impl2]]`.
                //
                // Without this arm, the dict would be stored as `pending_arm_pattern` (wrong),
                // and the close-bracket handler would emit "instance form has incomplete arm
                // (pattern without methods)" — causing parse errors for all prelude instances.
                if let SurfaceExpression::Dict(entries) = &node.expr {
                    for entry in entries {
                        current_arm_methods.push(entry.clone());
                    }
                    Ok(())
                } else {
                    // Non-dict expression after arm setup: treat as new arm pattern for
                    // multi-arm instances (e.g., `[instance Cls [patA]: methods [patB]: methods]`).
                    push_expr_to_parent(stack, current_document_items, node)
                }
            } else if pending_arm_pattern.is_none() {
                // No arms set up yet — this expression is a pattern (e.g., PatternDecl).
                // Delegate to push_expr_to_parent which sets pending_arm_pattern.
                push_expr_to_parent(stack, current_document_items, node)
            } else {
                // Unexpected expression while arm pattern is pending — delegate to parent.
                // Semantic validation is the resolver's job, not the parser's.
                push_expr_to_parent(stack, current_document_items, node)
            }
        }
        _ => {
            // All other frames: delegate to push_expr_to_parent
            push_expr_to_parent(stack, current_document_items, node)
        }
    }
}

/// Parse a single expression and return the first `SurfaceNode` from the first document.
///
/// Callers receive the native Surface AST; Display is handled by `SurfaceNode`/`SurfaceExpression`.
///
/// Returns the first item of the first document as an `Arc<SurfaceNode>`. For top-level
/// declaration items (`SurfaceItem::Decl`), wraps them in `SurfaceExpression::Decl` so
/// top-level declarations can be displayed uniformly. For callers needing the raw
/// `SurfaceProgram`, use `parse()` directly.
pub fn parse_surface_expression(input: &str) -> Result<Arc<SurfaceNode>, TypeDiagnostic> {
    let file: Arc<str> = Arc::from("<expression>");
    let output = parse(input, Arc::clone(&file))?;
    let surface = &output.program;

    if surface.documents.is_empty() {
        return Err(TypeDiagnostic::error(
            "parse-error",
            "no documents in input",
            crate::rust_span!(),
        ));
    }

    let first_doc = &surface.documents[0];
    match first_doc.node.items.first() {
        Some(SurfaceItem::Expr(node)) => Ok(Arc::clone(node)),
        Some(SurfaceItem::Decl(decl)) => {
            // Top-level declarations are wrapped in SurfaceExpression::Decl so they
            // can be displayed uniformly. The Display impl for SurfaceDeclaration
            // produces the canonical rendering matching Expr Display for the same forms.
            Ok(mk(
                SurfaceExpression::Decl(Box::new(decl.node.clone())),
                decl.span.clone(),
            ))
        }
        None => Err(TypeDiagnostic::error(
            "parse-error",
            "no items in first document",
            first_doc.span.clone(),
        )),
    }
}

/// Parse tinct source text with error recovery.
///
/// This function ALWAYS succeeds (returns a `ParseOutput`), even when there are parse errors.
/// Unlike `parse()` (which returns `Err` on fatal errors like lexer failure
/// or unclosed brackets at top level), this function converts all fatal errors into
/// `ParseOutput.diagnostics` and returns a synthetic AST.
///
/// Errors that occur inside bracket forms are recovered from: the parser substitutes
/// `SurfaceExpression::Error` nodes and continues. Fatal errors (lexer failure, unclosed brackets at
/// top level) are also recovered: they are recorded in `ParseOutput.diagnostics` and a minimal
/// empty `File` AST is returned.
///
/// Use this function when you want to report ALL parse errors at once (e.g. in an LSP
/// diagnostic pass or a batch linting tool) and always need an AST, even if it's empty.
pub fn parse_with_recovery(input: &str) -> ParseOutput {
    let file: Arc<str> = Arc::from("<recovery>");
    match parse(input, file) {
        Ok(output) => output,
        Err(fatal_diag) => {
            // Fatal error (lexer failure, unclosed brackets, etc.)
            // Construct a synthetic empty SurfaceProgram with the error recorded.
            let program = SurfaceProgram { documents: vec![] };
            ParseOutput {
                leading_comments: BTreeMap::new(),
                trailing_comments: BTreeMap::new(),
                blank_before: BTreeMap::new(),
                diagnostics: vec![fatal_diag],
                program,
            }
        }
    }
}

/// Format a parse diagnostic with Rust-style rich diagnostics (snippet + caret).
///
/// Produces output matching the format of `format_type_diagnostic`:
/// ```text
/// error: parse error message
///  --> file.llt:10:5
///   |
/// 10 | [invalid syntax here
///    |               ^^^^^
/// ```
pub fn format_parse_error(err: &TypeDiagnostic, source: &str, file_name: &str) -> String {
    use crate::error::render_span_snippet;

    // If there are no spans, fall back to basic message formatting
    if err.spans.is_empty() {
        return format!("error: {}", err.message);
    }

    let span = err.spans[0].0.clone();
    let line = span.start_line;
    let col = span.start_col;

    // Header: error: message
    let mut out = format!("error: {}\n", err.message);

    // Location: --> file:line:col
    out.push_str(&format!(" --> {file_name}:{line}:{col}\n"));

    // Snippet: source context with caret
    if let Some(snippet) = render_span_snippet(source, span) {
        out.push_str("  |\n");
        out.push_str(&snippet);
    }

    // Notes (e.g. "while parsing fn expression")
    for note in &err.notes {
        out.push_str(&format!("  = note: {note}\n"));
    }

    // Help line
    if let Some(help) = &err.help {
        out.push_str(&format!("  = help: {help}\n"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DiagnosticLevel;

    fn test_file(_src: &str) -> Arc<str> {
        Arc::from(file!())
    }

    /// Helper: parse successfully and return the first expression from the first document.
    /// Returns `Arc<SurfaceNode>` directly — no ast_convert bridge needed.
    fn parse_surf_node(input: &str) -> Arc<SurfaceNode> {
        let output = parse(input, test_file(input)).expect("parse failed");
        let first_doc = &output.program.documents[0].node;
        match first_doc.items.first() {
            Some(SurfaceItem::Expr(node)) => Arc::clone(node),
            _ => panic!("first item is not an expression"),
        }
    }

    /// Helper: extract all expression nodes from a SurfaceDocument.
    /// Returns `Vec<Arc<SurfaceNode>>` — no ast_convert bridge needed.
    fn surf_items(surf_doc: &SurfaceDocument) -> Vec<Arc<SurfaceNode>> {
        surf_doc
            .items
            .iter()
            .filter_map(|item| match item {
                SurfaceItem::Expr(node) => Some(Arc::clone(node)),
                SurfaceItem::Decl(_) => None,
            })
            .collect()
    }

    #[test]
    fn test_empty_dict() {
        let src = "[]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 0);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_one_value() {
        let src = "[42]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].node.key.is_none()); // auto-indexed
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(42)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_keyed_entry() {
        let src = "[a: 1 b: 2]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                // First entry: a: 1
                assert!(entries[0].node.key.is_some());
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(1)
                ));
                // Second entry: b: 2
                assert!(entries[1].node.key.is_some());
                match &entries[1].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
                assert!(matches!(
                    &entries[1].node.value.expr,
                    SurfaceExpression::Int(2)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_simple() {
        let src = "[call $f 1 2]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].expr, SurfaceExpression::Int(1)));
                assert!(matches!(&args[1].expr, SurfaceExpression::Int(2)));
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_named_args() {
        let src = "[call $f x: 1]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 1);
                assert_eq!(named_args[0].node.name, "x");
                assert!(matches!(
                    &named_args[0].node.value.expr,
                    SurfaceExpression::Int(1)
                ));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_simple() {
        let src = "[fn [let] 42]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                params,
                body,
                return_ann,
                desugared,
                resolved_captures: _,
            } => {
                assert_eq!(params.len(), 0);
                assert!(matches!(&body.expr, SurfaceExpression::Int(42)));
                assert!(return_ann.is_none());
                assert!(!*desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias() {
        let src = "[type 42]";
        let output = parse(src, test_file(src)).expect("parse failed");
        // [type ...] is a declaration — access via SurfaceItem::Decl, not expressions
        let items = &output.program.documents[0].node.items;
        assert_eq!(items.len(), 1, "expected one item");
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { params, body } => {
                    assert!(params.is_empty());
                    assert!(matches!(&body.expr, SurfaceExpression::Int(42)));
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }
    }

    /// Regression test for B-364: `[type ...]` in dict-entry-value position.
    ///
    /// The parser must correctly open a TypeAlias frame when `[type ...]` appears as
    /// the value of a keyed dict entry (e.g. `Foo: [type A B]`). The resulting AST
    /// must be `Dict([{key: Str("Foo"), value: Decl(TypeAlias { ... })}])`.
    ///
    /// Previously mis-labelled as a parser regression; the parser was always correct.
    /// The `#[ignore]` annotations on the corresponding typecheck tests were caused by
    /// T-951 enforcement (undeclared lowercase type variables) and multi-document
    /// test-helper limitations, not by a parser bug.
    #[test]
    fn test_type_alias_in_dict_entry_value() {
        // Single-entry: [Name: [type String]]
        let src = "[Name: [type String]]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected one expression item");
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected one dict entry");
                // Key must be Str("Name")
                match entries[0].node.key.as_ref().map(|k| &k.expr) {
                    Some(SurfaceExpression::StringLiteral { content: s, .. }) => {
                        assert_eq!(s, "Name")
                    }
                    other => panic!("expected Str key 'Name', got {other:?}"),
                }
                // Value must be Decl(TypeAlias)
                match &entries[0].node.value.expr {
                    SurfaceExpression::Decl(decl) => match decl.as_ref() {
                        SurfaceDeclaration::TypeAlias { params, body } => {
                            assert!(params.is_empty(), "expected no type params");
                            assert!(
                                matches!(&body.expr, SurfaceExpression::VarRef { name, .. } if name == "String"),
                                "expected VarRef(\"String\") body, got {:?}",
                                body.expr
                            );
                        }
                        other => panic!("expected TypeAlias declaration, got {other:?}"),
                    },
                    other => panic!("expected Decl value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }

        // Multi-entry union: [Result: [type A B C]]
        let src2 = "[Result: [type A B C]]";
        let output = parse(src2, test_file(src2)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match entries[0].node.key.as_ref().map(|k| &k.expr) {
                    Some(SurfaceExpression::StringLiteral { content: s, .. }) => {
                        assert_eq!(s, "Result")
                    }
                    other => panic!("expected Str key 'Result', got {other:?}"),
                }
                match &entries[0].node.value.expr {
                    SurfaceExpression::Decl(decl) => match decl.as_ref() {
                        SurfaceDeclaration::TypeAlias { params, body } => {
                            assert!(params.is_empty());
                            // Multi-entry body is a Dict with 3 positional entries
                            match &body.expr {
                                SurfaceExpression::Dict(body_entries) => {
                                    assert_eq!(body_entries.len(), 3, "expected 3 body entries");
                                    for entry in body_entries {
                                        assert!(
                                            entry.node.key.is_none(),
                                            "body entries must be positional"
                                        );
                                    }
                                }
                                other => panic!("expected Dict body, got {other:?}"),
                            }
                        }
                        other => panic!("expected TypeAlias, got {other:?}"),
                    },
                    other => panic!("expected Decl value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }

        // With [let ...] params and new named-key constructor syntax:
        // [Pair: [type [let a b] First: [first: a] Second: [second: b]]]
        let src3 = "[Pair: [type [let a b] First: [first: a] Second: [second: b]]]";
        let output = parse(src3, test_file(src3)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.expr {
                    SurfaceExpression::Decl(decl) => match decl.as_ref() {
                        SurfaceDeclaration::TypeAlias { params, body } => {
                            assert_eq!(params.len(), 2, "expected 2 type params (a, b)");
                            match &body.expr {
                                SurfaceExpression::Dict(body_entries) => {
                                    assert_eq!(body_entries.len(), 2, "expected 2 body entries");
                                }
                                other => panic!("expected Dict body, got {other:?}"),
                            }
                        }
                        other => panic!("expected TypeAlias, got {other:?}"),
                    },
                    other => panic!("expected Decl value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_literal_int() {
        let expr = parse_surf_node("42");
        assert!(matches!(expr.expr, SurfaceExpression::Int(42)));
    }

    #[test]
    fn test_depth_limit() {
        // MAX_PARSE_DEPTH + 1 opening brackets exceeds the limit.
        // parse() returns Err (depth-limit errors propagate as hard errors).
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('['); // One more to exceed
        for _ in 0..=MAX_PARSE_DEPTH {
            input.push(']');
        }

        let err = parse(&input, test_file(&input)).unwrap_err();
        assert!(
            err.message.contains("maximum nesting depth exceeded"),
            "expected depth-limit error message, got: {}",
            err.message
        );
    }

    /// Verify that nesting up to MAX_PARSE_DEPTH - 1 (200 levels below limit) succeeds.
    ///
    /// This regression test guards the lower boundary of the depth limit: inputs
    /// well below 256 must parse successfully. The parser is iterative (Vec<StackFrame>),
    /// so 200 levels creates no Rust call-stack pressure.
    #[test]
    fn test_depth_limit_well_below_maximum_succeeds() {
        const DEPTH: usize = 200;
        const {
            assert!(
                DEPTH < MAX_PARSE_DEPTH,
                "test depth must be less than MAX_PARSE_DEPTH"
            )
        };

        let mut input = String::new();
        for _ in 0..DEPTH {
            input.push('[');
        }
        input.push('1'); // innermost value
        for _ in 0..DEPTH {
            input.push(']');
        }

        let result = parse(&input, test_file(&input));
        assert!(
            result.is_ok(),
            "parsing {DEPTH} levels of nesting should succeed (limit is {MAX_PARSE_DEPTH}), got: {:?}",
            result.unwrap_err()
        );
    }

    /// Verify that exactly MAX_PARSE_DEPTH nesting levels succeeds.
    ///
    /// The depth check is `stack.len() >= MAX_PARSE_DEPTH` and fires BEFORE pushing,
    /// so the Nth bracket is processed when stack.len() = N-1. Therefore exactly
    /// MAX_PARSE_DEPTH brackets produces stack.len() = MAX_PARSE_DEPTH - 1 at the
    /// last push, which passes the check.
    #[test]
    fn test_depth_limit_at_exact_boundary_succeeds() {
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('1');
        for _ in 0..MAX_PARSE_DEPTH {
            input.push(']');
        }

        let result = parse(&input, test_file(&input));
        assert!(
            result.is_ok(),
            "exactly MAX_PARSE_DEPTH levels should succeed (check fires before push), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_type_assert_simple() {
        let src = "[@Number 42]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
                match &annotation.node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
                assert!(matches!(&expr.expr, SurfaceExpression::Int(42)));
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access_removed_parses_as_two_expressions() {
        // $a[0] parses as two separate expressions: VarRef("a") and Dict([Int(0)]).
        // The `[` is always OpenBracket — bracket access is not part of the grammar.
        let src = "$a[0]";
        let output = parse(src, test_file(src)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 2);
        match &items[0].expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
            other => panic!("expected VarRef, got {other:?}"),
        }
        match &items[1].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(0)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Error path tests ---
    //
    // Errors inside bracket forms are now recovered from: parse() returns Ok with
    // ParseOutput.diagnostics non-empty rather than returning Err. Tests use `output.diagnostics`.
    // Only top-level / structural errors (unmatched ], unclosed [, DocSeparator inside
    // brackets) remain as parse() returning Err.

    #[test]
    fn test_call_empty() {
        let src = "[call]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for empty call form"
        );
        assert!(
            output.diagnostics[0].message.contains("call form requires"),
            "expected error about call form requiring a function, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_call_func_as_named_arg() {
        // [call f: $x] — first arg is Named("f", ...) which is forbidden as func
        let src = "[call f: $x]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for named-arg func"
        );
        assert!(
            output.diagnostics[0].message.contains("named argument"),
            "expected error about named argument, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_dict_pending_key_no_value() {
        // [a:] — key with no value before closing bracket
        let src = "[a:]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for key without value"
        );
        assert!(
            output.diagnostics[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_call_pending_named_arg_no_value() {
        // [call $f x:] — named arg x with no value before closing bracket
        let src = "[call $f x:]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for named arg without value"
        );
        assert!(
            output.diagnostics[0].message.contains("without value"),
            "expected 'without value' error for named arg, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_type_alias_empty() {
        let src = "[type]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for empty type-alias"
        );
        assert!(
            output.diagnostics[0]
                .message
                .contains("type-alias form requires"),
            "expected error about type-alias requiring a type expression, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_type_assert_no_annotation() {
        // [@] — floating annotation with no value and no expression; AnnotationCollect
        // consumes the ] so the outer Dict bracket is unclosed → "unclosed bracket" error.
        let output = parse_with_recovery("[@]");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for bare @ with no annotation"
        );
        // The AnnotationCollect frame consumes the ] (its value or the close),
        // leaving the outer Dict bracket without a close.
        assert!(
            output.diagnostics[0].message.contains("unclosed bracket")
                || output.diagnostics[0].message.contains("extra annotation")
                || output.diagnostics[0].message.contains("annotation"),
            "expected error related to unclosed bracket or annotation, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_type_assert_no_expr() {
        // [@Number] — floating annotation but no expression to annotate → error
        let src = "[@Number]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for type-assert without expression"
        );
        assert!(
            output.diagnostics[0].message.contains("type-assert form")
                && output.diagnostics[0]
                    .message
                    .contains("requires an expression"),
            "expected error about missing expression, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_colon_outside_dict_call() {
        // [fn :] — "fn" not followed by colon directly → Fn form.
        // Then ":" in Fn frame → "`:` can only appear in dict, call, class, instance, match, let, or syntax-class forms" (recovered).
        let src = "[fn :]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for colon in fn form"
        );
        assert!(
            output.diagnostics[0].message.contains("key without value")
                || output.diagnostics[0].message.contains("`:` without a key")
                || output.diagnostics[0]
                    .message
                    .contains("`:` is not valid inside a")
                || output.diagnostics[0]
                    .message
                    .contains("`:` at document top level"),
            "expected key-related error for [fn :], got: {}",
            output.diagnostics[0].message
        );
        // Also test the true "colon outside dict/call" case: colon in a TypeAlias frame
        let src2 = "[type x :]";
        let output2 = parse(src2, test_file(src2)).expect("recovery should succeed");
        assert!(
            !output2.diagnostics.is_empty(),
            "expected recovered error for colon in type-alias form"
        );
        assert!(
            output2.diagnostics[0]
                .message
                .contains("`:` is not valid inside a type form")
                || output2.diagnostics[0]
                    .message
                    .contains("must follow an uppercase constructor name"),
            "expected error about colon in wrong context for [type x :], got: {}",
            output2.diagnostics[0].message
        );
    }

    #[test]
    fn test_colon_without_key_in_dict() {
        // [:] — colon with no preceding key in a dict
        let src = "[:]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for colon without key"
        );
        assert!(
            output.diagnostics[0].message.contains("`:` without a key"),
            "expected error about colon without key, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_fn_multiple_bodies() {
        // [fn [let] 1 2] — two body expressions in an fn form (Sequential wrapping)
        let src = "[fn [let] 1 2]";
        let output = parse(src, test_file(src)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "multi-expression fn bodies should parse successfully via Sequential, got errors: {:?}",
            output.diagnostics
        );
        // The fn body should be wrapped in SurfaceExpression::Sequential
        let items = surf_items(&output.program.documents[0].node);
        let expr = &items[0].expr;
        match expr {
            SurfaceExpression::Fn { body, .. } => match &body.expr {
                SurfaceExpression::Sequential(exprs) => {
                    assert_eq!(exprs.len(), 2, "expected 2 expressions in Sequential body");
                }
                other => panic!("expected Sequential body, got: {other:?}"),
            },
            other => panic!("expected Fn expression, got: {other:?}"),
        }
    }

    #[test]
    fn test_type_alias_multiple_exprs() {
        // [type 1 2] — multi-entry type-alias form (union declaration)
        let src = "[type 1 2]";
        let output = parse(src, test_file(src)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "multi-entry [type T1 T2 ...] should parse without errors, got: {:?}",
            output.diagnostics
        );
        // [type ...] is a declaration — access via SurfaceItem::Decl
        let items = &output.program.documents[0].node.items;
        assert_eq!(items.len(), 1, "expected one item");
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { params, body } => {
                    assert!(params.is_empty());
                    // Multi-entry body is wrapped in a synthetic Dict with positional entries
                    match &body.expr {
                        SurfaceExpression::Dict(entries) => {
                            assert_eq!(entries.len(), 2, "expected 2 positional entries");
                            assert!(
                                entries[0].node.key.is_none(),
                                "entries should be auto-indexed"
                            );
                            assert!(
                                entries[1].node.key.is_none(),
                                "entries should be auto-indexed"
                            );
                        }
                        other => {
                            panic!("expected Dict body for multi-entry type alias, got {other:?}")
                        }
                    }
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias_bare_name_params_rejected() {
        // [type [a b] Body] — bare-name params without `let` must be rejected
        let src = "[type [a b] Int]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected error for [type [a b] Int] (missing `let`), got no errors"
        );
        assert!(
            output.diagnostics[0].message.contains("must start with"),
            "expected error about missing `let`, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_type_alias_bare_name_single_param_rejected() {
        // [type [a] Body] — bare-name single param without `let` must be rejected
        let src = "[type [a] Int]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected error for [type [a] Int] (missing `let`), got no errors"
        );
        assert!(
            output.diagnostics[0].message.contains("must start with"),
            "expected error about missing `let`, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_type_alias_let_params_accepted() {
        // [type [let a b] Body] — `let`-wrapped params must be accepted
        let src = "[type [let a b] Int]";
        let output = parse(src, test_file(src)).expect("parse failed");
        assert!(
            output.diagnostics.is_empty(),
            "[type [let a b] Body] should parse without errors, got: {:?}",
            output.diagnostics
        );
        let items = &output.program.documents[0].node.items;
        assert_eq!(items.len(), 1, "expected one item");
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { params, .. } => {
                    assert_eq!(params.len(), 2, "expected 2 params");
                    assert_eq!(params[0], ("a".to_string(), None));
                    assert_eq!(params[1], ("b".to_string(), None));
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias_let_zero_params_accepted() {
        // [type [let] Body] — zero-param alias with explicit let bracket
        let output =
            parse("[type [let] Int]", test_file("[type [let] Int]")).expect("parse failed");
        assert!(
            output.diagnostics.is_empty(),
            "[type [let] Body] should parse without errors, got: {:?}",
            output.diagnostics
        );
        let items = &output.program.documents[0].node.items;
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { params, .. } => {
                    assert!(params.is_empty(), "expected 0 params, got {:?}", params);
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias_uppercase_type_expr_not_flagged() {
        // [type [or Int Null]] — type expression with uppercase names is NOT rejected as a bare-name param list
        let output =
            parse("[type [or Int Null]]", test_file("[type [or Int Null]]")).expect("parse failed");
        assert!(
            output.diagnostics.is_empty(),
            "[type [or Int Null]] should parse without errors, got: {:?}",
            output.diagnostics
        );
        let items = &output.program.documents[0].node.items;
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { params, .. } => {
                    assert!(
                        params.is_empty(),
                        "expected 0 params (type expr, not param list), got {:?}",
                        params
                    );
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }
    }

    #[test]
    fn test_type_assert_multiple_exprs() {
        // [@Number 1 2] — floating @Number with 2 positional entries in the bracket.
        // The TypeAssert unwrap only fires for single-entry dicts; with 2 entries the
        // Dict is returned as-is (floating annotation not applied to individual entries).
        let output =
            parse("[@Number 1 2]", test_file("[@Number 1 2]")).expect("parse should succeed");
        let items = &output.program.documents[0].node.items;
        assert_eq!(items.len(), 1, "expected one item");
        match &items[0] {
            SurfaceItem::Expr(node) => {
                match &node.expr {
                    SurfaceExpression::Dict(entries) => {
                        assert_eq!(
                            entries.len(),
                            2,
                            "expected 2 entries, got {}",
                            entries.len()
                        );
                    }
                    SurfaceExpression::TypeAssert { .. } => {
                        // Acceptable if TypeAssert wraps the whole expression
                    }
                    other => panic!("expected Dict or TypeAssert, got {other:?}"),
                }
            }
            other => panic!("expected Expr item, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_empty() {
        // [fn] — fn with no body
        let output = parse("[fn]", test_file("[fn]")).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for empty fn form"
        );
        assert!(
            output.diagnostics[0]
                .message
                .contains("fn form requires a body expression"),
            "expected error about fn requiring a body, got: {}",
            output.diagnostics[0].message
        );
    }

    // --- Edge case / positive tests ---

    #[test]
    fn test_keyword_as_dict_key() {
        // [call: 1] — "call" followed by colon → dict, not a call form (Fix 2)
        let output = parse("[call: 1]", test_file("[call: 1]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().expect("expected keyed entry");
                match &key.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "call"),
                    other => panic!("expected key 'call', got {other:?}"),
                }
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(1)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_all_keywords_as_dict_keys() {
        // [call: 1 fn: 2 type: 3] — all three keywords as dict keys
        let output = parse(
            "[call: 1 fn: 2 type: 3]",
            test_file("[call: 1 fn: 2 type: 3]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                let expected_keys = ["call", "fn", "type"];
                let expected_values = [1i64, 2, 3];
                for (i, (key, val)) in expected_keys.iter().zip(expected_values.iter()).enumerate()
                {
                    let entry_key = entries[i].node.key.as_ref().expect("expected keyed entry");
                    match &entry_key.expr {
                        SurfaceExpression::StringLiteral { content: s, .. } => {
                            assert_eq!(s.as_str(), *key)
                        }
                        other => panic!("expected key '{key}', got {other:?}"),
                    }
                    match &entries[i].node.value.expr {
                        SurfaceExpression::Int(n) => assert_eq!(*n, *val),
                        other => panic!("expected Int({val}), got {other:?}"),
                    }
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_whitespace_in_form_classification() {
        // "[ call $f]" — leading whitespace before keyword; peek skips it → still a Call form
        let output = parse("[ call $f]", test_file("[ call $f]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_keyed_entry_with_bracket_value() {
        // [a: [1]] — dict with keyed entry whose value is a nested dict (Fix 1)
        let output = parse("[a: [1]]", test_file("[a: [1]]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().expect("expected keyed entry");
                match &key.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                // Value should be a Dict containing Int(1)
                match &entries[0].node.value.expr {
                    SurfaceExpression::Dict(inner_entries) => {
                        assert_eq!(inner_entries.len(), 1);
                        assert!(matches!(
                            &inner_entries[0].node.value.expr,
                            SurfaceExpression::Int(1)
                        ));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_named_arg_bracket_value() {
        // [call $f x: [1]] — call with named arg whose value is a nested dict (Fix 1)
        let output =
            parse("[call $f x: [1]]", test_file("[call $f x: [1]]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 1);
                assert_eq!(named_args[0].node.name, "x");
                match &named_args[0].node.value.expr {
                    SurfaceExpression::Dict(inner_entries) => {
                        assert_eq!(inner_entries.len(), 1);
                        assert!(matches!(
                            &inner_entries[0].node.value.expr,
                            SurfaceExpression::Int(1)
                        ));
                    }
                    other => panic!("expected inner Dict for named arg value, got {other:?}"),
                }
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_only_named_args() {
        // [call $f x: 1 y: 2] — call with func and two named args, no positional
        let output =
            parse("[call $f x: 1 y: 2]", test_file("[call $f x: 1 y: 2]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 2);
                assert_eq!(named_args[0].node.name, "x");
                assert!(matches!(
                    &named_args[0].node.value.expr,
                    SurfaceExpression::Int(1)
                ));
                assert_eq!(named_args[1].node.name, "y");
                assert!(matches!(
                    &named_args[1].node.value.expr,
                    SurfaceExpression::Int(2)
                ));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_unmatched_closing_bracket() {
        let err = parse("]", test_file("]")).unwrap_err();
        assert!(
            err.message.contains("unmatched closing bracket"),
            "expected 'unmatched closing bracket' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_unclosed_bracket() {
        let err = parse("[", test_file("[")).unwrap_err();
        assert!(
            err.message.contains("unclosed bracket"),
            "expected 'unclosed bracket' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_colon_without_key() {
        // [call $f :] — colon inside Call frame with pending_key=None (no preceding identifier)
        let output =
            parse("[call $f :]", test_file("[call $f :]")).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for colon without name in call frame"
        );
        assert!(
            output.diagnostics[0].message.contains("without a name"),
            "expected error about colon without a name in call frame, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_annotation_invalid_token() {
        // [@123] — floating annotation with Int(123) as the annotation value, then no expression
        // → error because there's nothing to annotate.
        let output = parse("[@123]", test_file("[@123]")).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for floating annotation with no expression"
        );
        assert!(
            output.diagnostics[0].message.contains("type-assert form")
                || output.diagnostics[0]
                    .message
                    .contains("requires an expression"),
            "expected error about missing expression after annotation, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_mixed_keyed_and_auto_indexed() {
        // [a: 1 2 b: 3] — keyed, auto-indexed, keyed entries
        let output = parse("[a: 1 2 b: 3]", test_file("[a: 1 2 b: 3]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(
                    entries.len(),
                    3,
                    "expected 3 entries, got {}",
                    entries.len()
                );
                // Entry 0: key=Some("a"), value=1
                let key0 = entries[0]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 0 should have key");
                match &key0.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(1)
                ));
                // Entry 1: key=None (auto-indexed), value=2
                assert!(
                    entries[1].node.key.is_none(),
                    "entry 1 should be auto-indexed (no key)"
                );
                assert!(matches!(
                    &entries[1].node.value.expr,
                    SurfaceExpression::Int(2)
                ));
                // Entry 2: key=Some("b"), value=3
                let key2 = entries[2]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 2 should have key");
                match &key2.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
                assert!(matches!(
                    &entries[2].node.value.expr,
                    SurfaceExpression::Int(3)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- New tests for parser-core-c features ---

    #[test]
    fn test_fn_params_simple() {
        let output =
            parse("[fn [let x y] $x]", test_file("[fn [let x y] $x]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                params,
                body,
                return_ann,
                desugared,
                resolved_captures: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_none());
                assert!(!params[0].node.variadic);
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_none());
                assert!(!params[1].node.variadic);
                assert!(
                    matches!(&body.expr, SurfaceExpression::VarRef { name, .. } if name == "x")
                );
                assert!(return_ann.is_none());
                assert!(!desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_annotated() {
        let output =
            parse("[fn [let x@Int] $x]", test_file("[fn [let x@Int] $x]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                assert!(
                    matches!(
                        &params[0].node.annotation.as_ref().unwrap().node,
                        Annotation::Simple(s) if s == "Int"
                    ),
                    "expected Simple(\"Int\") annotation, got {:?}",
                    params[0].node.annotation.as_ref().unwrap().node
                );
                assert!(!params[0].node.variadic);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_return_annotation() {
        let output = parse(
            "[fn@Number [let x] $x]",
            test_file("[fn@Number [let x] $x]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                return_ann, params, ..
            } => {
                assert!(return_ann.is_some());
                match &return_ann.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_variadic() {
        let output = parse(
            "[fn [let ...args] $args]",
            test_file("[fn [let ...args] $args]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "args");
                assert!(params[0].node.variadic);
                assert!(params[0].node.annotation.is_none());
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_simple() {
        let output = parse("$a.b", test_file("$a.b")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Field { expr, field, .. } => {
                let inner = expr.as_ref().expect("expected Some(target)");
                match &inner.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert_eq!(*field, DotKey::Ident("b".to_string()));
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_chain() {
        let output = parse("$a.b.c", test_file("$a.b.c")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Field {
                expr: outer_expr,
                field: outer_field,
                ..
            } => {
                assert_eq!(*outer_field, DotKey::Ident("c".to_string()));
                let outer_inner = outer_expr
                    .as_ref()
                    .expect("expected Some(target) for outer");
                match &outer_inner.expr {
                    SurfaceExpression::Field {
                        expr: inner_expr,
                        field: inner_field,
                        ..
                    } => {
                        assert_eq!(*inner_field, DotKey::Ident("b".to_string()));
                        let inner_inner = inner_expr
                            .as_ref()
                            .expect("expected Some(target) for inner");
                        match &inner_inner.expr {
                            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
                            other => panic!("expected VarRef at base, got {other:?}"),
                        }
                    }
                    other => panic!("expected inner Field, got {other:?}"),
                }
            }
            other => panic!("expected outer Field, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_inside_call() {
        // [call $fn $a.b]
        let output = parse("[call $fn $a.b]", test_file("[call $fn $a.b]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "fn"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 1);
                match &args[0].expr {
                    SurfaceExpression::Field { expr, field, .. } => {
                        assert_eq!(*field, DotKey::Ident("b".to_string()));
                        let inner = expr.as_ref().expect("expected Some(target)");
                        match &inner.expr {
                            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
                            other => panic!("expected VarRef, got {other:?}"),
                        }
                    }
                    other => panic!("expected Field, got {other:?}"),
                }
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_inside_dict() {
        // [x: $y.z]
        let output = parse("[x: $y.z]", test_file("[x: $y.z]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].node.key.is_some());
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "x"),
                    other => panic!("expected key 'x', got {other:?}"),
                }
                match &entries[0].node.value.expr {
                    SurfaceExpression::Field { expr, field, .. } => {
                        assert_eq!(*field, DotKey::Ident("z".to_string()));
                        let inner = expr.as_ref().expect("expected Some(target)");
                        match &inner.expr {
                            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "y"),
                            other => panic!("expected VarRef, got {other:?}"),
                        }
                    }
                    other => panic!("expected Field, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_leading_dot_parent_scope() {
        // `.x` with no preceding expression — leading-dot form, expr: None
        let output = parse(".x", test_file(".x")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Field { expr, field, .. } => {
                assert!(expr.is_none(), "expected None target for leading-dot");
                assert_eq!(*field, DotKey::Ident("x".to_string()));
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn test_leading_dot_inside_dict() {
        // `[outer-x: .x]` — leading-dot inside a dict value
        let output = parse("[outer-x: .x]", test_file("[outer-x: .x]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.expr {
                    SurfaceExpression::Field { expr, field, .. } => {
                        assert!(
                            expr.is_none(),
                            "expected None target for leading-dot in dict value"
                        );
                        assert_eq!(*field, DotKey::Ident("x".to_string()));
                    }
                    other => panic!("expected Field, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_doc_separator() {
        let output =
            parse("[a: 1]\n---\n[b: 2]", test_file("[a: 1]\n---\n[b: 2]")).expect("parse failed");
        assert_eq!(output.program.documents.len(), 2);

        // First document
        let items1 = surf_items(&output.program.documents[0].node);
        assert_eq!(items1.len(), 1);
        match &items1[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc1, got {other:?}"),
        }

        // Second document
        let items2 = surf_items(&output.program.documents[1].node);
        assert_eq!(items2.len(), 1);
        match &items2[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc2, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_leading() {
        let output =
            parse("# comment\n[a: 1]", test_file("# comment\n[a: 1]")).expect("parse failed");
        assert!(!output.leading_comments.is_empty());
        // Comments are attached by offset of next significant token
        // We just verify that we have at least one comment collected
        let has_comment = output
            .leading_comments
            .values()
            .any(|v| v.iter().any(|c| c.contains("comment")));
        assert!(
            has_comment,
            "expected to find 'comment' in leading_comments"
        );
    }

    #[test]
    fn test_fn_param_variadic_not_last() {
        // [let ...args x] — `...` param followed by a plain param. The parser accepts
        // any parameter ordering.
        let result = parse(
            "[fn [let ...args x] $x]",
            test_file("[fn [let ...args x] $x]"),
        );
        assert!(
            result.is_ok(),
            "parser should accept variadic in non-last position"
        );
    }

    #[test]
    fn test_fn_multiple_variadic() {
        // [let ...args ...rest] — two `...` params. The parser accepts any number of
        // `...`-prefixed params in any position.
        let result = parse(
            "[fn [let ...args ...rest] $x]",
            test_file("[fn [let ...args ...rest] $x]"),
        );
        assert!(result.is_ok(), "parser should accept multiple variadics");
    }

    #[test]
    fn test_range_outside_bracket_access() {
        // `..` emits two consecutive Dot tokens. `1..5` lexes as Int(1), Dot, Dot, Int(5).
        // The first Dot triggers dot-access on Int(1) but the next token is another Dot
        // (not an identifier) → parse error at top level.
        // At top level (stack empty), the parser returns Err rather than recovering.
        let err = parse("1..5", test_file("1..5")).unwrap_err();
        assert!(
            err.message.contains("expected field name") || err.message.contains("found Dot"),
            "expected a dot-access parse error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_doc_separator_inside_bracket() {
        // --- inside a bracket expression
        let err = parse("[---]", test_file("[---]")).unwrap_err();
        assert!(
            err.message
                .contains("document separator cannot appear inside"),
            "expected error about doc separator inside bracket, got: {}",
            err.message
        );
    }

    // --- Tests added for review findings (parser-core-c1) ---

    #[test]
    fn test_whitespace_allows_dot_access() {
        // "$a .b" has whitespace before dot; dot access is not whitespace-sensitive (unlike '['),
        // so this parses as a single DotAccess expression (same as "$a.b").
        let output = parse("$a .b", test_file("$a .b")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(
            items.len(),
            1,
            "expected 1 expression (Field), got {}",
            items.len()
        );
        match &items[0].expr {
            SurfaceExpression::Field {
                expr: inner_opt,
                field,
                ..
            } => {
                assert_eq!(*field, DotKey::Ident("b".to_string()));
                let inner = inner_opt.as_ref().expect("expected Some(target)");
                match &inner.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
                    other => panic!("expected VarRef('a') inside Field, got {other:?}"),
                }
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_parses_as_separate_dict() {
        // "$a [0]" parses as two separate expressions: VarRef and Dict([Int(0)])
        let output = parse("$a [0]", test_file("$a [0]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(
            items.len(),
            2,
            "expected 2 expressions (VarRef 'a' + Dict containing Int(0)), got {}",
            items.len()
        );
        match &items[0].expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "a"),
            other => panic!("expected VarRef('a') as first expr, got {other:?}"),
        }
        match &items[1].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(
                    &entries[0].node.value.expr,
                    SurfaceExpression::Int(0)
                ));
            }
            other => panic!("expected Dict([Int(0)]) as second expr, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_mixed() {
        // [fn [let x y@Int ...rest] $x] — simple + annotated + variadic
        let output = parse(
            "[fn [let x y@Int ...rest] $x]",
            test_file("[fn [let x y@Int ...rest] $x]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, body, .. } => {
                assert_eq!(params.len(), 3);
                // param 0: simple "x"
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_none());
                assert!(!params[0].node.variadic);
                // param 1: annotated "y@Int"
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_some());
                assert!(
                    matches!(
                        &params[1].node.annotation.as_ref().unwrap().node,
                        Annotation::Simple(s) if s == "Int"
                    ),
                    "expected Simple(\"Int\") annotation, got {:?}",
                    params[1].node.annotation.as_ref().unwrap().node
                );
                assert!(!params[1].node.variadic);
                // param 2: variadic "...rest"
                assert_eq!(params[2].node.name, "rest");
                assert!(params[2].node.variadic);
                assert!(params[2].node.annotation.is_none());
                // body
                assert!(
                    matches!(&body.expr, SurfaceExpression::VarRef { name, .. } if name == "x")
                );
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_both_annotations() {
        // [fn@Number [let x@Int] $x] — return annotation + annotated param
        let output = parse(
            "[fn@Number [let x@Int] $x]",
            test_file("[fn@Number [let x@Int] $x]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                params,
                return_ann,
                body,
                ..
            } => {
                // Return annotation
                assert!(return_ann.is_some());
                match &return_ann.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple(Number) return annotation, got {other:?}"),
                }
                // Param
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                assert!(
                    matches!(
                        &params[0].node.annotation.as_ref().unwrap().node,
                        Annotation::Simple(s) if s == "Int"
                    ),
                    "expected Simple(\"Int\") annotation, got {:?}",
                    params[0].node.annotation.as_ref().unwrap().node
                );
                assert!(!params[0].node.variadic);
                // Body
                assert!(
                    matches!(&body.expr, SurfaceExpression::VarRef { name, .. } if name == "x")
                );
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_on_dict_literal() {
        // "[x: 1].x" — dot access immediately after closing bracket (no whitespace)
        // The lexer emits Dot (access operator) after ']' since CloseBracket is in access context.
        let output = parse("[x: 1].x", test_file("[x: 1].x")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected 1 expression (Field)");
        match &items[0].expr {
            SurfaceExpression::Field { expr, field, .. } => {
                assert_eq!(*field, DotKey::Ident("x".to_string()));
                let inner = expr.as_ref().expect("expected Some(target)");
                match &inner.expr {
                    SurfaceExpression::Dict(entries) => {
                        assert_eq!(entries.len(), 1);
                        match &entries[0].node.key.as_ref().unwrap().expr {
                            SurfaceExpression::StringLiteral { content: s, .. } => {
                                assert_eq!(s, "x")
                            }
                            other => panic!("expected key 'x', got {other:?}"),
                        }
                    }
                    other => panic!("expected Dict as Field target, got {other:?}"),
                }
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_trailing() {
        // "[a: 1] # trailing comment" — comment on same line as dict → trailing
        let output = parse(
            "[a: 1] # trailing comment",
            test_file("[a: 1] # trailing comment"),
        )
        .expect("parse failed");
        assert!(
            !output.trailing_comments.is_empty(),
            "expected at least one trailing comment"
        );
        let has_comment = output
            .trailing_comments
            .values()
            .any(|c| c.contains("trailing comment"));
        assert!(
            has_comment,
            "expected to find 'trailing comment' in trailing_comments, got: {:?}",
            output.trailing_comments
        );
    }

    #[test]
    fn test_multiple_doc_separators() {
        // Three documents separated by two ---
        let output = parse(
            "[a: 1]\n---\n[b: 2]\n---\n[c: 3]",
            test_file("[a: 1]\n---\n[b: 2]\n---\n[c: 3]"),
        )
        .expect("parse failed");
        assert_eq!(
            output.program.documents.len(),
            3,
            "expected 3 documents, got {}",
            output.program.documents.len()
        );

        // Document 1: [a: 1]
        let items1 = surf_items(&output.program.documents[0].node);
        assert_eq!(items1.len(), 1);
        match &items1[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "a"),
                    other => panic!("expected key 'a' in doc1, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc1, got {other:?}"),
        }

        // Document 2: [b: 2]
        let items2 = surf_items(&output.program.documents[1].node);
        assert_eq!(items2.len(), 1);
        match &items2[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "b"),
                    other => panic!("expected key 'b' in doc2, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc2, got {other:?}"),
        }

        // Document 3: [c: 3]
        let items3 = surf_items(&output.program.documents[2].node);
        assert_eq!(items3.len(), 1);
        match &items3[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "c"),
                    other => panic!("expected key 'c' in doc3, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc3, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_empty_params() {
        // [fn [let] 42] — fn with explicit empty param list, body Int(42)
        let output = parse("[fn [let] 42]", test_file("[fn [let] 42]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                params,
                body,
                return_ann,
                desugared,
                resolved_captures: _,
            } => {
                assert_eq!(params.len(), 0, "expected empty param list");
                assert!(matches!(&body.expr, SurfaceExpression::Int(42)));
                assert!(return_ann.is_none());
                assert!(!desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_param_span() {
        // [fn [let x@Int] $x] — verify param[0] span covers "x@Int"
        // "[fn [let x@Int] $x]"
        //  column 1234567890123456789
        //  column 10 = 'x', column 11 = '@', column 12..14 = "Int"
        let output =
            parse("[fn [let x@Int] $x]", test_file("[fn [let x@Int] $x]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                let param_span = params[0].span.clone();
                assert_eq!(
                    param_span.start_col, 10,
                    "expected param span to start at column 10 ('x'), got {}",
                    param_span.start_col
                );
                assert!(
                    param_span.end_col > 13,
                    "expected param span end col > 13 (includes '@Int'), got {}",
                    param_span.end_col
                );
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_key() {
        let output =
            parse("[a: 1  a: 2]", test_file("[a: 1  a: 2]")).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for duplicate key"
        );
        assert!(
            output.diagnostics[0].message.contains("duplicate key"),
            "expected 'duplicate key' in error, got: {}",
            output.diagnostics[0].message
        );
        assert!(
            output.diagnostics[0].message.contains("\"a\""),
            "expected key name in error, got: {}",
            output.diagnostics[0].message
        );
    }

    #[test]
    fn test_empty_document_explicit() {
        // --- is the LLT document separator
        let output = parse("---\n[a: 1]", test_file("---\n[a: 1]")).expect("parse failed");
        assert_eq!(output.program.documents.len(), 2);
        assert_eq!(output.program.documents[0].node.expressions().count(), 0);
        assert_eq!(output.program.documents[1].node.expressions().count(), 1);
    }

    #[test]
    fn test_annotated_bare_word() {
        let expr = parse_surf_node("word@Int");
        match &expr.expr {
            // Annotation is stored directly on VarRef as Simple("Int").
            SurfaceExpression::VarRef {
                name,
                annotation: Some(annotation),
                ..
            } => {
                assert_eq!(name.as_str(), "word");
                match &annotation.node {
                    Annotation::Simple(s) => assert_eq!(s, "Int"),
                    other => panic!("expected Simple(\"Int\") annotation, got {other:?}"),
                }
            }
            other => panic!("expected annotated VarRef, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_collection() {
        let output =
            parse("# my comment\n[a: 1]", test_file("# my comment\n[a: 1]")).expect("parse failed");
        assert!(
            !output.leading_comments.is_empty(),
            "expected leading_comments to be non-empty"
        );
        let has_comment = output
            .leading_comments
            .values()
            .any(|comments| comments.iter().any(|c| c.contains("my comment")));
        assert!(
            has_comment,
            "expected to find 'my comment' in leading_comments, got: {:?}",
            output.leading_comments
        );
    }

    /// Regression test: bracket forms should have correct line and column numbers in their spans.
    /// Previously, all bracket forms had line:1 col:1 regardless of actual position.
    #[test]
    fn test_bracket_form_span_line_column() {
        // Dict on line 4 (after 3 comment lines)
        let input = "# Line 1\n# Line 2\n# Line 3\n[x: 10\n y: 20]";
        let output = parse(input, test_file(input)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        // The first item is the dict; its span should start at line 4, column 1
        assert_eq!(items[0].span.start_line, 4, "Dict should start on line 4");
        assert_eq!(items[0].span.start_col, 1, "Dict should start at column 1");

        // Also test a nested bracket form
        let input2 = "# Line 1\n[outer: [inner: 1]]";
        let output2 = parse(input2, test_file(input2)).expect("parse failed");
        let items2 = surf_items(&output2.program.documents[0].node);
        match &items2[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                // Outer dict starts on line 2
                assert_eq!(
                    items2[0].span.start_line, 2,
                    "Outer dict should start on line 2"
                );
                // Inner dict should also have correct line/column (line 2, after "outer: ")
                match &entries[0].node.value.expr {
                    SurfaceExpression::Dict(_) => {
                        let inner_span = entries[0].node.value.span.clone();
                        assert_eq!(
                            inner_span.start_line, 2,
                            "Inner dict should start on line 2"
                        );
                        assert_eq!(
                            inner_span.start_col, 9,
                            "Inner dict should start at column 9 (after 'outer: ')"
                        );
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    /// Regression test: Call, Fn, TypeAlias, and TypeAssert bracket forms
    /// should have correct line and column numbers in their spans.
    #[test]
    fn test_bracket_form_span_variants() {
        // Call form on line 3
        let input_call = "# Line 1\n# Line 2\n[call $f 1]";
        let output = parse(input_call, test_file(input_call)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Call { .. } => {
                assert_eq!(items[0].span.start_line, 3, "Call should start on line 3");
            }
            other => panic!("expected Call, got {other:?}"),
        }

        // Fn form on line 3
        let input_fn = "# Line 1\n# Line 2\n[fn [let x] $x]";
        let output = parse(input_fn, test_file(input_fn)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { .. } => {
                assert_eq!(items[0].span.start_line, 3, "Fn should start on line 3");
            }
            other => panic!("expected Fn, got {other:?}"),
        }

        // TypeAlias form on line 3 — [type ...] is a declaration, access via SurfaceItem::Decl
        let input_type = "# Line 1\n# Line 2\n[type Int]";
        let output = parse(input_type, test_file(input_type)).expect("parse failed");
        let items = &output.program.documents[0].node.items;
        assert_eq!(items.len(), 1, "expected one item");
        match &items[0] {
            SurfaceItem::Decl(decl) => match &decl.node {
                SurfaceDeclaration::TypeAlias { .. } => {
                    assert_eq!(decl.span.start_line, 3, "TypeAlias should start on line 3");
                }
                other => panic!("expected TypeAlias declaration, got {other:?}"),
            },
            other => panic!("expected Decl item, got {other:?}"),
        }

        // TypeAssert form on line 3
        let input_assert = "# Line 1\n# Line 2\n[@Int 42]";
        let output = parse(input_assert, test_file(input_assert)).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::TypeAssert { .. } => {
                assert_eq!(
                    items[0].span.start_line, 3,
                    "TypeAssert should start on line 3"
                );
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_call_newline_colon_not_dict() {
        // [call\n: x] — newline before colon should not create dict with "call" key
        // Instead, it's a call form with zero args followed by unexpected colon (recovered)
        let output =
            parse("[call\n: x]", test_file("[call\n: x]")).expect("recovery should succeed");
        assert!(
            !output.diagnostics.is_empty(),
            "expected recovered error for colon without name in call form"
        );
        assert!(
            output.diagnostics[0].message.contains("`:` without a name"),
            "expected error about colon without name, got: {}",
            output.diagnostics[0].message
        );
    }

    // --- Error recovery tests (Items 2-5 of parser-error-recovery sprint) ---

    /// A single error inside brackets is recovered from: parse() returns Ok, the
    /// document contains an SurfaceExpression::Error node, and ParseOutput.errors has one entry.
    #[test]
    fn test_recovery_single_error_inside_brackets() {
        // [a:] — key without value; recovered with SurfaceExpression::Error node
        let src = "[a:]";
        let output = parse(src, test_file(src)).expect("recovery should succeed");
        assert_eq!(
            output.diagnostics.len(),
            1,
            "expected exactly 1 recovered error"
        );
        assert!(
            output.diagnostics[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.diagnostics[0].message
        );
        // The document should contain one expression (the SurfaceExpression::Error node)
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected 1 expression (Error node)");
        assert!(
            matches!(items[0].expr, SurfaceExpression::Error(_)),
            "expected SurfaceExpression::Error node after recovery, got: {:?}",
            items[0].expr
        );
    }

    /// Multiple errors are all collected: parse() returns Ok with multiple entries in
    /// ParseOutput.errors, and the document contains multiple SurfaceExpression::Error nodes.
    #[test]
    fn test_recovery_multiple_errors() {
        // Two consecutive broken bracket forms at document level
        let output = parse("[a:] [b:]", test_file("[a:] [b:]")).expect("recovery should succeed");
        assert_eq!(
            output.diagnostics.len(),
            2,
            "expected 2 recovered errors, got {:?}",
            output
                .diagnostics
                .iter()
                .map(|e| &e.message)
                .collect::<Vec<_>>()
        );
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(
            items.len(),
            2,
            "expected 2 expressions (2 Error nodes), got {}",
            items.len()
        );
        assert!(
            matches!(items[0].expr, SurfaceExpression::Error(_)),
            "expected first expression to be SurfaceExpression::Error"
        );
        assert!(
            matches!(items[1].expr, SurfaceExpression::Error(_)),
            "expected second expression to be SurfaceExpression::Error"
        );
    }

    /// An error in a nested bracket is recovered from, and the outer bracket continues
    /// to parse normally. The outer dict should contain an Error node as its value.
    #[test]
    fn test_recovery_error_in_nested_brackets() {
        // [outer: [inner:]] — inner bracket has key without value; outer should still parse
        let output = parse("[outer: [inner:]]", test_file("[outer: [inner:]]"))
            .expect("recovery should succeed");
        assert_eq!(output.diagnostics.len(), 1, "expected 1 recovered error");
        assert!(
            output.diagnostics[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.diagnostics[0].message
        );
        // The outer dict should have one entry
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected 1 top-level expression");
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected 1 outer entry");
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "outer"),
                    other => panic!("expected key 'outer', got {other:?}"),
                }
                // The value should be the SurfaceExpression::Error from the inner bracket
                assert!(
                    matches!(entries[0].node.value.expr, SurfaceExpression::Error(_)),
                    "expected SurfaceExpression::Error as outer value, got: {:?}",
                    entries[0].node.value.expr
                );
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    /// parse_with_recovery is an infallible wrapper that always returns ParseOutput.
    /// Valid input produces ParseOutput.errors=[] and a well-formed AST.
    #[test]
    fn test_parse_with_recovery_valid_input() {
        let output = parse_with_recovery("[a: 1 b: 2]");
        assert!(
            output.diagnostics.is_empty(),
            "expected no errors for valid input, got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].expr, SurfaceExpression::Dict(_)));
    }

    /// parse_with_recovery on errored input returns ParseOutput with errors collected.
    #[test]
    fn test_parse_with_recovery_error_input() {
        let output = parse_with_recovery("[fn]");
        assert_eq!(output.diagnostics.len(), 1, "expected 1 recovered error");
        assert!(
            output.diagnostics[0]
                .message
                .contains("fn form requires a body"),
            "expected fn-body error, got: {}",
            output.diagnostics[0].message
        );
    }

    /// parse_with_recovery on fatal errors (unclosed brackets) returns synthetic empty File.
    #[test]
    fn test_parse_with_recovery_fatal_error() {
        let output = parse_with_recovery("[");
        assert_eq!(output.diagnostics.len(), 1, "expected 1 fatal error");
        assert!(
            output.diagnostics[0].message.contains("unclosed bracket"),
            "expected unclosed-bracket error, got: {}",
            output.diagnostics[0].message
        );
        assert_eq!(
            output.program.documents.len(),
            0,
            "expected empty documents for fatal error"
        );
    }

    /// Task 1: Partial dict preservation - valid entries before error are kept.
    #[test]
    fn test_recovery_partial_dict_preservation() {
        // [a: 1  a: 2] — has one valid entry (a: 1) before the duplicate key error
        // Should recover with a partial dict containing the valid entry plus an error entry
        let output =
            parse("[a: 1  a: 2]", test_file("[a: 1  a: 2]")).expect("recovery should succeed");
        assert_eq!(
            output.diagnostics.len(),
            1,
            "expected exactly 1 recovered error"
        );
        assert!(
            output.diagnostics[0].message.contains("duplicate key"),
            "expected 'duplicate key' error, got: {}",
            output.diagnostics[0].message
        );

        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected 1 expression");

        // The recover_from_bracket_error() function builds a partial dict when there are
        // valid entries, adding an error entry for the failed part
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                // Should have 2 entries: the valid "a: 1" plus the error entry
                assert_eq!(entries.len(), 2, "expected 2 entries (1 valid + 1 error)");

                // First entry should be a: 1
                match &entries[0].node.key.as_ref().unwrap().expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => {
                        assert_eq!(s, "a", "expected key 'a'")
                    }
                    other => panic!("expected key 'a', got {other:?}"),
                }
                match &entries[0].node.value.expr {
                    SurfaceExpression::Int(n) => assert_eq!(*n, 1, "expected value 1"),
                    other => panic!("expected value 1, got {other:?}"),
                }

                // Second entry should be an error (auto-indexed, no key)
                assert!(
                    entries[1].node.key.is_none(),
                    "expected error entry to have no key"
                );
                assert!(
                    matches!(entries[1].node.value.expr, SurfaceExpression::Error(_)),
                    "expected SurfaceExpression::Error as second entry value"
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// With the new general annotation mechanism, any expression is valid as an annotation value.
    /// [@123 x: 1] is now valid: floating @123 (Int in annotation position) applies to x's value.
    #[test]
    fn test_recovery_annotation_invalid_token() {
        // [@123 x: 1] — with new mechanism, any expression is valid in annotation position.
        // @123 becomes a floating annotation (PropertyDict wrapping Int(123)) applied to value 1.
        let output = parse_with_recovery("[@123 x: 1]");
        assert_eq!(output.program.documents.len(), 1, "expected 1 document");
        let items = surf_items(&output.program.documents[0].node);
        assert!(!items.is_empty(), "expected at least 1 expression");
        // The result is a Dict with the annotation applied to the value of x
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected 1 entry");
            }
            SurfaceExpression::Error(_) => {
                // Also acceptable if the form produced an error
            }
            other => panic!("expected Dict or Error, got {other:?}"),
        }
    }

    /// skip_to_closing_bracket correctly finds the matching ] accounting for nesting.
    #[test]
    fn test_skip_to_closing_bracket() {
        // Tokenize "[a [b c] d]" and verify skip_to_closing_bracket from index 1
        // (just past the opening '[') finds the matching ']' at the end.
        let src = "[a [b c] d]";
        let tokens = crate::lexer::tokenize(src, test_file(src)).expect("tokenize failed");
        // tokens: [ Identifier("a") OpenBracket Identifier("b") Identifier("c") CloseBracket Identifier("d") CloseBracket
        // from_idx=1 (Identifier("a")), depth starts at 1
        let close = skip_to_closing_bracket(&tokens, 1);
        assert!(
            close < tokens.len(),
            "expected to find closing bracket, got tokens.len()"
        );
        assert!(
            matches!(tokens[close].node, crate::lexer::Token::CloseBracket),
            "expected CloseBracket at close index, got {:?}",
            tokens[close].node
        );
        // The outer ']' should be the last token
        assert_eq!(
            close,
            tokens.len() - 1,
            "expected close to be the last token"
        );
    }

    #[test]
    fn test_underscore_exclusion_positions() {
        // Verify that $_ in key position ([$_: 42]) is parsed as VarRef.
        // Bracket access and range access are not part of the grammar.

        // $_ in dict key position: [$_: 42]
        let expr = parse_surf_node("[$_: 42]");
        match &expr.expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key_expr = entries[0].node.key.as_ref().expect("expected key");
                match &key_expr.expr {
                    SurfaceExpression::VarRef { name, .. } => {
                        assert_eq!(name, "_", "$_ as dict key should be VarRef")
                    }
                    other => panic!("expected VarRef(_) for dict key, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Interpolated string tmpl-call tests ---

    /// i"Hello $name" parses as StringLiteral with raw content (desugar handles tmpl expansion).
    #[test]
    fn test_desugar_interpolated_string_varref() {
        let expr = parse_surf_node(r#"i"Hello $name""#);
        assert!(
            matches!(&expr.expr, SurfaceExpression::StringLiteral { prefix, delimiter, content }
                if prefix == "i" && delimiter == "\"" && content == "Hello $name"),
            "expected StringLiteral with raw i-string content, got {:?}",
            expr.expr
        );
    }

    /// i"${[+ $x 1]}" parses as StringLiteral with raw content. The ${...} form is not supported —
    /// desugar passes `${[+ $x 1]}` through literally in the template string.
    #[test]
    fn test_desugar_interpolated_string_expr() {
        let expr = parse_surf_node(r#"i"${[+ $x 1]}""#);
        assert!(
            matches!(&expr.expr, SurfaceExpression::StringLiteral { prefix, delimiter, content }
                if prefix == "i" && delimiter == "\"" && content == "${[+ $x 1]}"),
            "expected StringLiteral with raw i-string content, got {:?}",
            expr.expr
        );
    }

    /// i"prefix $name suffix ${[+ $x 1]} end" — parser stores raw content; desugar expands $name
    /// but passes ${...} through literally since ${expr} interpolation is not supported.
    #[test]
    fn test_desugar_interpolated_string_mixed() {
        let expr = parse_surf_node(r#"i"prefix $name suffix ${[+ $x 1]} end""#);
        assert!(
            matches!(&expr.expr, SurfaceExpression::StringLiteral { prefix, delimiter, content }
                if prefix == "i" && delimiter == "\"" && content == "prefix $name suffix ${[+ $x 1]} end"),
            "expected StringLiteral with raw i-string content, got {:?}",
            expr.expr
        );
    }

    /// i"foo ${bar" lexes successfully with raw content — the ${...} form is not validated at
    /// lex/parse time and is passed through literally by desugar.
    #[test]
    fn test_desugar_interpolated_string_expr_unclosed() {
        // The lexer stores raw content; ${...} is not a special form.
        // i"foo ${bar" closes at the " after bar, yielding raw content "foo ${bar".
        let expr = parse_surf_node(r#"i"foo ${bar""#);
        assert!(
            matches!(&expr.expr, SurfaceExpression::StringLiteral { prefix, delimiter, content }
                if prefix == "i" && delimiter == "\"" && content == "foo ${bar"),
            "expected StringLiteral with raw content, got {:?}",
            expr.expr
        );
    }

    // --- Pipe (|) operator parsing tests ---

    /// `a | b` parses as Pipe { lhs: VarRef("a"), rhs: VarRef("b") }.
    #[test]
    fn test_pipe_basic() {
        let expr = parse_surf_node("a | b");
        match &expr.expr {
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                assert!(
                    matches!(&lhs.expr, SurfaceExpression::VarRef { name, .. } if name == "a"),
                    "expected lhs = VarRef(a), got {:?}",
                    lhs.expr
                );
                assert!(
                    matches!(&rhs.expr, SurfaceExpression::VarRef { name, .. } if name == "b"),
                    "expected rhs = VarRef(b), got {:?}",
                    rhs.expr
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `a | b | c` is left-associative: parsed as `(a | b) | c`.
    #[test]
    fn test_pipe_left_assoc() {
        let expr = parse_surf_node("a | b | c");
        match &expr.expr {
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                // rhs must be VarRef("c")
                assert!(
                    matches!(&rhs.expr, SurfaceExpression::VarRef { name, .. } if name == "c"),
                    "expected rhs = VarRef(c), got {:?}",
                    rhs.expr
                );
                // lhs must be Pipe { a | b }
                match &lhs.expr {
                    SurfaceExpression::Pipe {
                        lhs: inner_lhs,
                        rhs: inner_rhs,
                        ..
                    } => {
                        assert!(
                            matches!(&inner_lhs.expr, SurfaceExpression::VarRef { name, .. } if name == "a"),
                            "expected inner_lhs = VarRef(a), got {:?}",
                            inner_lhs.expr
                        );
                        assert!(
                            matches!(&inner_rhs.expr, SurfaceExpression::VarRef { name, .. } if name == "b"),
                            "expected inner_rhs = VarRef(b), got {:?}",
                            inner_rhs.expr
                        );
                    }
                    other => panic!("expected nested Pipe for lhs, got {other:?}"),
                }
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `$x | [f $y]` — pipe where RHS is an explicit Call bracket expression.
    /// Note: dot access (`.`) has higher precedence than pipe (`|`). So `a.b | c.d`
    /// parses as `Field(Pipe(Field(a,b), c), d)` — the trailing `.d` extends
    /// beyond the pipe. To test pipe with a bracket RHS, use an explicit `[...]` form.
    #[test]
    fn test_pipe_inside_brackets() {
        // $x | [f $y] — top-level pipe, RHS is an explicit Call
        let expr = parse_surf_node("$x | [f $y]");
        match &expr.expr {
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                assert!(
                    matches!(&lhs.expr, SurfaceExpression::VarRef { name, .. } if name == "x"),
                    "expected lhs = VarRef(x), got {:?}",
                    lhs.expr
                );
                assert!(
                    matches!(&rhs.expr, SurfaceExpression::Call { .. }),
                    "expected rhs = Call, got {:?}",
                    rhs.expr
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `a.b | c.d` — dot access on both sides of pipe.
    #[test]
    fn test_pipe_dot_then_pipe() {
        let expr = parse_surf_node("$data.name | upper");
        match &expr.expr {
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                assert!(
                    matches!(&lhs.expr, SurfaceExpression::Field { .. }),
                    "expected lhs = Field, got {:?}",
                    lhs.expr
                );
                assert!(
                    matches!(&rhs.expr, SurfaceExpression::VarRef { name, .. } if name == "upper"),
                    "expected rhs = VarRef(upper), got {:?}",
                    rhs.expr
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    // --- DotKey::Int parsing tests ---

    /// `$a.0` parses as Field with DotKey::Int(0).
    #[test]
    fn test_dot_access_int_key() {
        let expr = parse_surf_node("$a.0");
        match &expr.expr {
            SurfaceExpression::Field { field, .. } => {
                assert!(
                    matches!(field, DotKey::Int(0)),
                    "expected DotKey::Int(0), got {:?}",
                    field
                );
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    /// `$a.0.name` parses as chained Field: outer is Ident("name"), inner is Int(0).
    #[test]
    fn test_dot_access_int_then_ident() {
        let expr = parse_surf_node("$a.0.name");
        match &expr.expr {
            SurfaceExpression::Field {
                expr: target,
                field,
                ..
            } => {
                // outer field: "name"
                assert!(
                    matches!(field, DotKey::Ident(s) if s == "name"),
                    "expected outer DotKey::Ident(name), got {:?}",
                    field
                );
                // inner: Field on $a with Int(0)
                match &target
                    .as_ref()
                    .expect("inner Field should have Some(target)")
                    .expr
                {
                    SurfaceExpression::Field {
                        field: inner_field, ..
                    } => {
                        assert!(
                            matches!(inner_field, DotKey::Int(0)),
                            "expected inner DotKey::Int(0), got {:?}",
                            inner_field
                        );
                    }
                    other => panic!("expected inner Field, got {other:?}"),
                }
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    /// `%cwd` in value position parses as `VarRef("%cwd")`.
    /// `%` is a plain bare-word character — no special-case path in the lexer or parser.
    #[test]
    fn test_percent_cwd_as_varref() {
        let expr = parse_surf_node("%cwd");
        match &expr.expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "%cwd"),
            other => panic!("expected VarRef(\"%cwd\"), got {other:?}"),
        }
    }

    /// `%nc` parses as `VarRef("%nc")` — injected cap names work uniformly.
    #[test]
    fn test_percent_nc_as_varref() {
        let expr = parse_surf_node("%nc");
        match &expr.expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "%nc"),
            other => panic!("expected VarRef(\"%nc\"), got {other:?}"),
        }
    }

    /// Bare `%` parses as `VarRef("%")` — the pipeline input variable.
    #[test]
    fn test_percent_bare_as_varref() {
        let expr = parse_surf_node("%");
        match &expr.expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "%"),
            other => panic!("expected VarRef(\"%\"), got {other:?}"),
        }
    }

    /// `%cwd.field` parses as `Field { expr: VarRef("%cwd"), field: DotKey::Ident("field") }`.
    /// The `%cwd` identifier is consumed as one token; `.` emits Dot; `field` is the access field.
    #[test]
    fn test_percent_cwd_dot_access() {
        let expr = parse_surf_node("%cwd.field");
        match &expr.expr {
            SurfaceExpression::Field {
                expr: inner_opt,
                field,
                ..
            } => {
                assert_eq!(*field, DotKey::Ident("field".to_string()));
                let inner = inner_opt.as_ref().expect("expected Some(target)");
                match &inner.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "%cwd"),
                    other => panic!("expected VarRef(\"%cwd\") inside Field, got {other:?}"),
                }
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    /// `[open %cwd "Cargo.toml" "r"]` parses as a Call with `%cwd` as a positional arg.
    #[test]
    fn test_percent_cwd_as_call_arg() {
        let expr = parse_surf_node("[open %cwd \"Cargo.toml\" \"r\"]");
        match &expr.expr {
            SurfaceExpression::Call { func, args, .. } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "open"),
                    other => panic!("expected VarRef(\"open\") as func, got {other:?}"),
                }
                assert_eq!(args.len(), 3);
                // First arg: %cwd (args contains Arc<SurfaceNode> directly)
                match &args[0].expr {
                    SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "%cwd"),
                    other => panic!("expected VarRef(\"%cwd\") as first arg, got {other:?}"),
                }
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    /// `$a.0.1` parses as chained Field with two Int keys (not Float 0.1).
    /// Regression test: the lexer must suppress float detection after access-dot,
    /// otherwise `0.1` would be lexed as a single Float token.
    #[test]
    fn test_dot_access_int_chain() {
        let expr = parse_surf_node("$a.0.1");
        match &expr.expr {
            SurfaceExpression::Field {
                expr: target,
                field,
                ..
            } => {
                // outer field: Int(1)
                assert!(
                    matches!(field, DotKey::Int(1)),
                    "expected outer DotKey::Int(1), got {:?}",
                    field
                );
                // inner: Field on $a with Int(0)
                match &target
                    .as_ref()
                    .expect("inner Field should have Some(target)")
                    .expr
                {
                    SurfaceExpression::Field {
                        field: inner_field, ..
                    } => {
                        assert!(
                            matches!(inner_field, DotKey::Int(0)),
                            "expected inner DotKey::Int(0), got {:?}",
                            inner_field
                        );
                    }
                    other => panic!("expected inner Field, got {other:?}"),
                }
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn test_format_parse_error() {
        // Create a parse error with a span — use unclosed bracket which is always fatal
        let source = "[a: 1";
        let err = parse(source, test_file(source)).unwrap_err();

        // Format it with the new function
        let formatted = format_parse_error(&err, source, "test.llt");

        // Should contain key elements:
        // - "error:" prefix
        // - file name and location
        // - source snippet
        assert!(formatted.starts_with("error:"));
        assert!(formatted.contains("test.llt"));
        assert!(formatted.contains("-->"));
        // The snippet should include the source line
        assert!(formatted.contains("[a: 1"));
        // Unclosed bracket should include a help line
        assert!(
            formatted.contains("= help:"),
            "unclosed bracket error should include a help line, got:\n{formatted}"
        );
        assert!(
            formatted.contains("add a closing ]"),
            "help line should mention adding a closing bracket, got:\n{formatted}"
        );
    }

    #[test]
    fn test_format_parse_error_fn_params_help() {
        // [fn [x y] body] — missing `let` in params — should include help text
        let source = "[fn [x y] x]";
        let output = parse(source, test_file(source)).expect("parse should produce output");
        assert!(
            !output.diagnostics.is_empty(),
            "expected a parse diagnostic for [fn [x y] body]"
        );
        let diag = &output.diagnostics[0];
        let formatted = format_parse_error(diag, source, "test.llt");
        assert!(
            formatted.contains("= help:"),
            "fn param error should include a help line, got:\n{formatted}"
        );
        assert!(
            formatted.contains("[fn [let x y] body]"),
            "help should show the correct form, got:\n{formatted}"
        );
    }

    #[test]
    fn test_format_parse_error_no_span() {
        // Create a parse diagnostic without spans (empty spans vec → no-span fallback)
        let err =
            TypeDiagnostic::with_spans(DiagnosticLevel::Err, "parse-error", "test error", vec![]);

        // Format it
        let formatted = format_parse_error(&err, "dummy source", "test.llt");

        // Should fall back to simple formatting
        assert_eq!(formatted, "error: test error");
        assert!(!formatted.contains("-->"));
    }

    #[test]
    fn test_fn_params_letdecl_simple() {
        // Test [fn [let x y] body] — LetDecl as parameter list
        let output = parse(
            "[fn [let x y] [+ $x $y]]",
            test_file("[fn [let x y] [+ $x $y]]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn {
                params,
                body,
                return_ann,
                desugared,
                resolved_captures: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_none());
                assert!(!params[0].node.variadic);
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_none());
                assert!(!params[1].node.variadic);
                assert!(matches!(&body.expr, SurfaceExpression::Call { .. })); // [+ $x $y]
                assert!(return_ann.is_none());
                assert!(!desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_letdecl_annotated() {
        // Test [fn [let x@Int y] body] — LetDecl with annotations
        let output = parse(
            "[fn [let x@Int y] [+ $x $y]]",
            test_file("[fn [let x@Int y] [+ $x $y]]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                assert!(
                    matches!(
                        &params[0].node.annotation.as_ref().unwrap().node,
                        Annotation::Simple(s) if s == "Int"
                    ),
                    "expected Simple(\"Int\") annotation, got {:?}",
                    params[0].node.annotation.as_ref().unwrap().node
                );
                assert!(!params[0].node.variadic);
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_none());
                assert!(!params[1].node.variadic);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_letdecl_mixed() {
        // Test [fn [let x@Int y@String z] body]
        let output = parse(
            "[fn [let x@Int y@String z] $x]",
            test_file("[fn [let x@Int y@String z] $x]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 3);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_some());
                assert_eq!(params[2].node.name, "z");
                assert!(params[2].node.annotation.is_none());
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_letdecl_with_placeholder() {
        // Test [fn [let x ...y] body] — x is a plain param, ...y is variadic.
        // In [let ...], whitespace between ... and the name is insignificant:
        // `... y` parses identically to `...y` (both create a variadic param named y).
        let output =
            parse("[fn [let x ...y] $x]", test_file("[fn [let x ...y] $x]")).expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].node.name, "x");
                assert!(!params[0].node.variadic);
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.variadic);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_letdecl_params_eval_typed() {
        // [fn [let x@Int y] body] — typed params via [let ...] form parse correctly.
        // Verifies parse_param_list recognises [let ...], extracts x (annotated @Int)
        // and y (unannotated) as SurfaceParam entries with correct annotation shapes.
        // Eval-level coverage: tests/corpus/eval/typed_fn_params.llt-eval.
        let output = parse(
            "[fn [let x@Int y] [+ $x $y]]",
            test_file("[fn [let x@Int y] [+ $x $y]]"),
        )
        .expect("parse failed");
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Fn { params, .. } => {
                assert_eq!(params.len(), 2, "expected 2 params");
                assert_eq!(params[0].node.name, "x");
                assert!(
                    params[0].node.annotation.is_some(),
                    "x should have @Int annotation"
                );
                assert!(
                    matches!(
                        &params[0].node.annotation.as_ref().unwrap().node,
                        Annotation::Simple(s) if s == "Int"
                    ),
                    "expected Simple(\"Int\") annotation, got {:?}",
                    params[0].node.annotation.as_ref().unwrap().node
                );
                assert_eq!(params[1].node.name, "y");
                assert!(
                    params[1].node.annotation.is_none(),
                    "y should be unannotated"
                );
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_output_as_surface_program() {
        // Test the ParseOutput::as_surface_program() bridge method.
        let output = parse("[a: 1  b: 2]", test_file("[a: 1  b: 2]")).expect("parse failed");
        let surface = output.as_surface_program();

        // Should have one document
        assert_eq!(surface.documents.len(), 1);

        // Document should have one item (the dict)
        let doc = &surface.documents[0].node;
        assert_eq!(doc.items.len(), 1);

        // Item should be a Dict expression
        match &doc.items[0] {
            crate::ast::SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    crate::ast::SurfaceExpression::Dict(entries) if entries.len() == 2
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    /// Regression test: `[let [let x y]]` — nested `[let ...]` inside a `[let ...]` context.
    ///
    /// Before the Token::Let fix, when inside a LetDecl frame the context-sensitive rule fired
    /// for ANY `[`, including `[let ...]`. This caused the `let` keyword of the inner bracket
    /// to be processed as VarRef("let"), producing `LetDecl { bindings: ["let", x, y] }` —
    /// a three-element LetDecl with "let" as the first binding.
    ///
    /// After the fix: when `next_is_let` is true, the context-sensitive rule is skipped and
    /// the standard Token::Let dispatch fires instead. The inner `[let x y]` becomes a proper
    /// LetDecl { bindings: [x, y] }, which is then the sole binding of the outer LetDecl.
    #[test]
    fn test_let_nested_let_inner_is_proper_letdecl() {
        // [let [let x y]] — outer LetDecl with one binding: an inner LetDecl
        let output = parse("[let [let x y]]", test_file("[let [let x y]]")).expect("parse failed");
        assert!(
            output.diagnostics.is_empty(),
            "expected no parse errors for [let [let x y]], got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected one top-level expression");

        // Outer LetDecl
        match &items[0].expr {
            SurfaceExpression::LetDecl { bindings } => {
                assert_eq!(
                    bindings.len(),
                    1,
                    "outer LetDecl should have exactly 1 binding (the inner [let x y]), got {:?}",
                    bindings.iter().map(|b| &b.expr).collect::<Vec<_>>()
                );
                // The sole binding must be an inner LetDecl (not VarRef("let"))
                match &bindings[0].expr {
                    SurfaceExpression::LetDecl {
                        bindings: inner_bindings,
                    } => {
                        assert_eq!(
                            inner_bindings.len(),
                            2,
                            "inner LetDecl should have 2 bindings (x, y), got {:?}",
                            inner_bindings.iter().map(|b| &b.expr).collect::<Vec<_>>()
                        );
                        match &inner_bindings[0].expr {
                            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "x"),
                            other => panic!("expected VarRef('x'), got {other:?}"),
                        }
                        match &inner_bindings[1].expr {
                            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "y"),
                            other => panic!("expected VarRef('y'), got {other:?}"),
                        }
                    }
                    other => panic!(
                        "expected inner LetDecl as sole binding of outer LetDecl; \
                         before the Token::Let fix this would be VarRef(\"let\") followed by x,y. \
                         Got: {other:?}"
                    ),
                }
            }
            other => panic!("expected outer LetDecl, got {other:?}"),
        }
    }

    /// Regression test: `[@Integer _]` in a match arm must not leave a stale AnnotationCollect
    /// frame on the stack.
    ///
    /// Before the fix: the Dict CloseBracket handler for the floating-annotation / TypeAssert path
    /// called `push_value(TypeAssert node)` then did `i += 1; continue`, bypassing the
    /// `drain_annotation_frames` call at line ~2619. If this `[@Integer _]` appeared inside a
    /// Match frame and the Match's parent was a Dict, the AnnotationCollect was already fully
    /// consumed (target=Floating, value set) — but because the Dict frame consumed the floating
    /// annotation internally and then immediately closed, there was nothing left on the stack to
    /// drain. The real bug is more subtle: the AnnotationCollect for `@Integer` is pushed BEFORE
    /// `[@Integer _]` opens a Dict sub-frame. After `[@Integer _]` closes via the early-continue
    /// path, control returns to the Match frame — but only via `push_value(TypeAssert)`, which
    /// stores it in `pending_pattern_expr`. The AnnotationCollect was already drained when `Integer`
    /// was pushed into the AnnotationCollect's value field. The actual issue: after the TypeAssert
    /// early-continue, any outer AnnotationCollect waiting for THIS TypeAssert as its value is
    /// not drained. In the test-loader.llt case the outer context is a Match frame (no annotation
    /// collect above it), so the TypeAssert lands in `pending_pattern_expr` correctly — BUT if
    /// another complete AnnotationCollect happened to be on the stack (e.g. from a different `@`
    /// earlier that didn't drain), that stale frame would intercept subsequent `:` or value tokens.
    ///
    /// This test exercises the minimal reproducer from test-loader.llt line 64:
    ///   `[fn [let b] [match b [@Integer ...]: b ...: 1]]`
    #[test]
    fn test_type_assert_in_match_arm_pattern() {
        // This is the minimal form of bool->int from test-loader.llt line 64.
        // [@Integer _] is a TypeAssert pattern in a match arm — it must parse cleanly.
        let src_type_assert =
            "[x: [fn [let b] [match b Boolean.False: 0 [@Integer ...]: b ...: 1]] x]";
        let output =
            parse(src_type_assert, test_file(src_type_assert)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "expected no parse errors, got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1, "expected 1 top-level expression (Dict)");
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 2, "expected 2 entries: x: <fn> and x");
                // First entry: x: [fn [let b] [match b ...]]
                let key0 = entries[0]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 0 should have key");
                match &key0.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => assert_eq!(s, "x"),
                    other => panic!("expected key 'x', got {other:?}"),
                }
                assert!(
                    matches!(&entries[0].node.value.expr, SurfaceExpression::Fn { .. }),
                    "expected Fn as value of 'x', got {:?}",
                    entries[0].node.value.expr
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// Regression test: after `[@Integer _]: b` completes a match arm, the next dict key
    /// must be parseable. This isolates the `:` error that manifests in test-loader.llt.
    /// The parse must succeed and produce the expected Match with 3 arms.
    #[test]
    fn test_type_assert_match_arm_followed_by_keyed_entry() {
        // This matches the shape of the test-loader.llt failure: after a fn containing a
        // match with [@Type _] arm, the NEXT entry in the outer dict must parse correctly.
        let src_arm_keyed = "[bool->int: [fn [let b] [match b Boolean.False: 0 [@Integer ...]: b ...: 1]] typecheck-docs: 42]";
        let output = parse(src_arm_keyed, test_file(src_arm_keyed)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "expected no parse errors for match-with-type-assert followed by keyed entry, got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        assert_eq!(items.len(), 1);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(
                    entries.len(),
                    2,
                    "expected 2 entries: bool->int and typecheck-docs"
                );
                // Verify the second key is "typecheck-docs"
                let key1 = entries[1]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 1 should have key");
                match &key1.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => {
                        assert_eq!(s, "typecheck-docs")
                    }
                    other => panic!("expected key 'typecheck-docs', got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// `key@[doc: "..."]: value` — dict entry where key has a property-dict annotation.
    /// This is used extensively in prelude.llt (e.g., `lines@[doc: "..."]:  [fn ...]`).
    #[test]
    fn test_annotated_key_property_dict() {
        let src_annotated_key = r#"[lines@[doc: "Read all lines."]: [fn [let h] h]  other: 42]"#;
        let output =
            parse(src_annotated_key, test_file(src_annotated_key)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "expected no errors for key@[annotation]: val, got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 2, "expected 2 entries");
                let key0 = entries[0]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 0 should have key");
                assert!(
                    matches!(&key0.expr, SurfaceExpression::VarRef { name, annotation: Some(_), .. } if name == "lines"),
                    "expected annotated VarRef key 'lines', got {:?}",
                    key0.expr
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// Minimal repro: fn@[multiline-annotation] loses Fn frame.
    #[test]
    fn test_fn_multiline_annotation_does_not_lose_frame() {
        let src = "[whenM: [fn@[return: Unknown  doc: \"\"\"\nMultiline doc.\n\"\"\"] [let app cond action]\n  [if cond action [app.pure []]]]  other: 42]";
        let output = parse_with_recovery(src);
        assert!(
            output.diagnostics.is_empty(),
            "fn@[multiline-annotation] lost its frame: {:?}",
            output.diagnostics
        );
    }

    /// Repro: fn@[bind: [a]  return: a] nested type param in annotation.
    #[test]
    fn test_fn_annotation_with_nested_type_param() {
        let src = "[f@[doc: \"x\"]: [fn@[bind: [a]  return: a] [let p d xs] xs]  other: 42]";
        let output = parse_with_recovery(src);
        assert!(
            output.diagnostics.is_empty(),
            "fn@[bind: [a] return: a] lost its frame: {:?}",
            output.diagnostics
        );
    }

    /// `key@[return: T  doc: """..."""]: value` — multiline triple-quoted doc string in annotation.
    /// This is the exact pattern used in prelude.llt for functions like `range`, `repeat`, etc.
    #[test]
    fn test_annotated_key_multiline_doc() {
        let src = "[range@[return: Integer  doc: \"\"\"\nMultiline doc.\n\"\"\"]: [fn [let x] x]  other: 42]";
        let output = parse(src, test_file(src)).expect("parse should succeed");
        assert!(
            output.diagnostics.is_empty(),
            "expected no errors for key@[multiline-annotation]: val, got: {:?}",
            output.diagnostics
        );
        let items = surf_items(&output.program.documents[0].node);
        match &items[0].expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 2, "expected 2 entries");
                let key0 = entries[0]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 0 should have key");
                assert!(
                    matches!(&key0.expr, SurfaceExpression::VarRef { name, annotation: Some(_), .. } if name == "range"),
                    "expected annotated VarRef key 'range', got {:?}",
                    key0.expr
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_expression_to_annotation_expr_produces_quote() {
        let node = SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: "Expr".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            crate::ast::Span::rust_source(file!(), line!()),
        );
        let ann = expression_to_annotation(&node);
        assert!(
            matches!(ann, Annotation::Quote),
            "expected Annotation::Quote for @Expr, got {ann:?}"
        );
    }

    #[test]
    fn test_expression_to_annotation_non_expr_produces_simple() {
        let node = SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: "Integer".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            crate::ast::Span::rust_source(file!(), line!()),
        );
        let ann = expression_to_annotation(&node);
        assert!(
            matches!(ann, Annotation::Simple(ref s) if s == "Integer"),
            "expected Annotation::Simple(\"Integer\") for @Integer, got {ann:?}"
        );
    }
}
