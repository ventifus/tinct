//! Pest PEG parser: converts source text into a fully-spanned AST.

use std::cell::RefCell;

use pest::Parser;
use pest_derive::Parser;

use crate::ast::*;

#[derive(Parser)]
#[grammar = "grammar.pest"]
pub(crate) struct LltParser;

/// Maximum nesting depth for bracket expressions.
/// Enforced during AST construction, not during pest's parse phase.
/// Pest recurses on Rust's call stack, so ~500+ nested brackets can
/// overflow before this check fires. See Phase 6 (hand-written parser).
const MAX_PARSE_DEPTH: usize = 256;

/// Parse LLT source text into a spanned File AST.
/// No input length limit is enforced; callers should validate input size if needed.
pub fn parse(input: &str) -> Result<Spanned<File>, ParseError> {
    let pairs = LltParser::parse(Rule::file, input).map_err(|e| ParseError {
        message: format!("{e}"),
        span: None,
    })?;

    let file_pair = pairs
        .into_iter()
        .next()
        .expect("grammar guarantees file rule produces a pair");
    let lines = LineTable::new(input);
    let file_span = make_span(&file_pair, &lines);

    let mut documents = Vec::new();

    for pair in file_pair.into_inner() {
        match pair.as_rule() {
            Rule::document => {
                documents.push(build_document(pair, &lines)?);
            }
            Rule::doc_separator | Rule::EOI => {}
            other => unreachable!("unexpected rule in file: {other:?}"),
        }
    }

    // Empty file → one document with no expressions
    if documents.is_empty() {
        documents.push(Spanned::new(
            Document {
                expressions: vec![],
            },
            Span {
                start: Position {
                    offset: 0,
                    line: 1,
                    column: 1,
                },
                end: lines.offset_to_position(input.len()),
            },
        ));
    }

    Ok(Spanned::new(File { documents }, file_span))
}

/// Parse a single expression from the first document, for convenience in tests and simple cases.
///
/// When the input contains multiple sequential expressions within a single document,
/// this function returns only the **last** expression (mirroring LLT's scope-chain
/// semantics where each expression in a document can shadow the previous one).
pub fn parse_expression(input: &str) -> Result<Spanned<Expr>, ParseError> {
    let file = parse(input)?;
    let doc = &file.node.documents[0];
    if doc.node.expressions.is_empty() {
        Ok(Spanned::new(Expr::Dict(vec![]), doc.span))
    } else if doc.node.expressions.len() == 1 {
        Ok(doc.node.expressions[0].clone())
    } else {
        // Multiple expressions: return the last one
        Ok(doc
            .node
            .expressions
            .last()
            .expect("len > 1 so last() is Some")
            .clone())
    }
}

fn build_document(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
) -> Result<Spanned<Document>, ParseError> {
    let span = make_span(&pair, lines);
    let mut expressions = Vec::new();

    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::expression {
            let expr_pair = inner
                .into_inner()
                .next()
                .expect("grammar guarantees expression has inner value");
            expressions.push(build_value(expr_pair, lines, 0)?);
        }
    }

    Ok(Spanned::new(Document { expressions }, span))
}

/// Error returned when parsing fails, including message and optional source location.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Option<Span>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            write!(
                f,
                "{}:{}: {}",
                span.start.line, span.start.column, self.message
            )
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ParseError {}

fn make_span(pair: &pest::iterators::Pair<'_, Rule>, lines: &LineTable) -> Span {
    let pest_span = pair.as_span();
    Span {
        start: lines.offset_to_position(pest_span.start()),
        end: lines.offset_to_position(pest_span.end()),
    }
}

/// Pre-computed line-offset table for O(log n) offset-to-position lookups.
#[derive(Debug)]
struct LineTable {
    /// Byte offset of the start of each line (line 1 = index 0).
    line_starts: Vec<usize>,
}

impl LineTable {
    fn new(input: &str) -> Self {
        let mut line_starts = vec![0];
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                line_starts.push(i + 2);
                i += 2;
            } else if bytes[i] == b'\n' {
                line_starts.push(i + 1);
                i += 1;
            } else {
                i += 1;
            }
        }
        LineTable { line_starts }
    }

    fn offset_to_position(&self, offset: usize) -> Position {
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        Position {
            offset,
            line: line_index + 1,
            column: offset - self.line_starts[line_index] + 1,
        }
    }
}

fn build_value(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    debug_assert!(depth <= MAX_PARSE_DEPTH);
    if depth >= MAX_PARSE_DEPTH {
        return Err(ParseError {
            message: format!("maximum nesting depth exceeded ({MAX_PARSE_DEPTH})"),
            span: Some(make_span(&pair, lines)),
        });
    }

    match pair.as_rule() {
        Rule::value => {
            let inner = pair
                .into_inner()
                .next()
                .expect("grammar guarantees value has inner pair");
            build_value(inner, lines, depth)
        }
        Rule::atom => {
            let inner = pair
                .into_inner()
                .next()
                .expect("grammar guarantees atom has inner pair");
            build_value(inner, lines, depth)
        }
        Rule::int_lit => {
            let span = make_span(&pair, lines);
            let n: i64 = pair.as_str().parse().map_err(|e| ParseError {
                message: format!("invalid integer: {e}"),
                span: Some(span),
            })?;
            Ok(Spanned::new(Expr::Int(n), span))
        }
        Rule::float_lit => {
            let span = make_span(&pair, lines);
            let n: f64 = pair.as_str().parse().map_err(|e| ParseError {
                message: format!("invalid float: {e}"),
                span: Some(span),
            })?;
            Ok(Spanned::new(Expr::Float(n), span))
        }
        Rule::bool_lit => {
            let span = make_span(&pair, lines);
            let b = pair.as_str() == "true";
            Ok(Spanned::new(Expr::Bool(b), span))
        }
        Rule::quoted_string => {
            let span = make_span(&pair, lines);
            let inner = pair
                .into_inner()
                .find(|p| p.as_rule() == Rule::inner_string)
                .map(|p| unescape(p.as_str()))
                .unwrap_or_default();
            Ok(Spanned::new(Expr::Str(inner), span))
        }
        Rule::var_ref => {
            let span = make_span(&pair, lines);
            let name = pair
                .as_str()
                .strip_prefix('$')
                .expect("grammar guarantees var_ref starts with $")
                .to_string();
            Ok(Spanned::new(Expr::VarRef(name), span))
        }
        Rule::bare_word => {
            let span = make_span(&pair, lines);
            Ok(Spanned::new(Expr::Str(pair.as_str().to_string()), span))
        }
        Rule::annotated_bare => build_annotated_bare(pair, lines, depth),
        Rule::bracket_expr => build_bracket_expr(pair, lines, depth + 1),
        Rule::access_expr => build_access_expr(pair, lines, depth),
        Rule::bare_token => {
            let inner = pair
                .into_inner()
                .next()
                .expect("grammar guarantees bare_token has inner pair");
            build_value(inner, lines, depth)
        }
        rule => Err(ParseError {
            message: format!("unexpected rule in value position: {rule:?}"),
            span: Some(make_span(&pair, lines)),
        }),
    }
}

fn build_bracket_expr(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    let span = make_span(&pair, lines);
    let mut inner = pair.into_inner().peekable();

    let first = match inner.peek() {
        None => return Ok(Spanned::new(Expr::Dict(vec![]), span)), // empty: []
        Some(p) => p,
    };

    match first.as_rule() {
        Rule::type_assert_body => {
            let first = inner.next().expect("peek succeeded so next is Some");
            build_type_assert(first, span, lines, depth)
        }
        Rule::special_form => {
            let sf = inner
                .next()
                .expect("peek succeeded so next is Some")
                .into_inner()
                .next()
                .expect("grammar guarantees special_form has inner pair");
            build_special_form(sf, span, lines, depth)
        }
        Rule::dict_entries => {
            let first = inner.next().expect("peek succeeded so next is Some");
            let entries = build_dict_entries(first, lines, depth)?;
            Ok(Spanned::new(Expr::Dict(entries), span))
        }
        rule => Err(ParseError {
            message: format!("unexpected rule inside bracket_expr: {rule:?}"),
            span: Some(span),
        }),
    }
}

fn build_type_assert(
    pair: pest::iterators::Pair<'_, Rule>,
    span: Span,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    // Grammar: type_assert_body = { "@" ~ annotation_value ~ value }
    let mut inner = pair.into_inner();
    let ann_pair = inner
        .next()
        .expect("grammar guarantees type_assert_body has annotation_value");
    let expr_pair = inner
        .next()
        .expect("grammar guarantees type_assert_body has value");

    let annotation = build_annotation_value(ann_pair, lines, depth)?;
    let expr = build_value(expr_pair, lines, depth)?;

    Ok(Spanned::new(
        Expr::TypeAssert {
            annotation,
            expr: Box::new(expr),
            resolved_type: RefCell::new(None),
        },
        span,
    ))
}

fn build_annotated_bare(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    // Grammar: annotated_bare = ${ bare_word ~ "@" ~ annotation_value }
    let span = make_span(&pair, lines);
    let mut inner = pair.into_inner();
    let name_pair = inner
        .next()
        .expect("grammar guarantees annotated_bare has bare_word");
    let name = name_pair.as_str().to_string();
    let ann_pair = inner
        .next()
        .expect("grammar guarantees annotated_bare has annotation_value");
    let annotation = build_annotation_value(ann_pair, lines, depth)?;
    Ok(Spanned::new(Expr::Annotated { name, annotation }, span))
}

fn build_special_form(
    pair: pest::iterators::Pair<'_, Rule>,
    span: Span,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    match pair.as_rule() {
        Rule::call_form => build_call(pair, span, lines, depth),
        Rule::fn_form => build_fn(pair, span, lines, depth),
        Rule::type_form => build_type_alias(pair, span, lines, depth),
        rule => Err(ParseError {
            message: format!("unexpected special form: {rule:?}"),
            span: Some(span),
        }),
    }
}

fn build_call(
    pair: pest::iterators::Pair<'_, Rule>,
    span: Span,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    // Grammar: call_form = { keyword_call ~ value ~ call_args }
    let mut inner = pair.into_inner();

    let _ = inner.next().expect("grammar guarantees keyword_call");

    let func = build_value(
        inner
            .next()
            .expect("grammar guarantees call_form has function value"),
        lines,
        depth,
    )?;

    let call_args_pair = inner
        .next()
        .expect("grammar guarantees call_form has call_args");
    let mut args = Vec::new();
    let mut named_args = Vec::new();

    for child in call_args_pair.into_inner() {
        match child.as_rule() {
            Rule::named_arg => {
                let na = build_named_arg(child, lines, depth)?;
                named_args.push(na);
            }
            _ => {
                let val = build_value(child, lines, depth)?;
                args.push(val);
            }
        }
    }

    Ok(Spanned::new(
        Expr::Call {
            func: Box::new(func),
            args,
            named_args,
        },
        span,
    ))
}

fn build_named_arg(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<NamedArg>, ParseError> {
    let span = make_span(&pair, lines);
    let mut inner = pair.into_inner();
    let key_pair = inner.next().expect("grammar guarantees named_arg has key");
    let name = key_pair.as_str().to_string();
    // named_arg_key can match either `$var` (var_ref) or `bare_word`;
    // strip the `$` prefix so the stored name is always bare.
    let name = name.strip_prefix('$').map(String::from).unwrap_or(name);

    let value = build_value(
        inner
            .next()
            .expect("grammar guarantees named_arg has value"),
        lines,
        depth,
    )?;

    Ok(Spanned::new(NamedArg { name, value }, span))
}

fn build_fn(
    pair: pest::iterators::Pair<'_, Rule>,
    span: Span,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    // Grammar: fn_form = { keyword_fn ~ fn_annotation? ~ param_list ~ value }
    let mut inner = pair.into_inner();

    let _ = inner.next().expect("grammar guarantees keyword_fn");

    let mut return_ann = None;
    let mut next = inner
        .next()
        .expect("grammar guarantees fn_form has token after keyword");

    // Check for fn_annotation
    if next.as_rule() == Rule::fn_annotation {
        let ann_inner = next
            .into_inner()
            .next()
            .expect("grammar guarantees fn_annotation has annotation_value");
        return_ann = Some(build_annotation_value(ann_inner, lines, depth)?);
        next = inner
            .next()
            .expect("grammar guarantees fn_form has param_list after annotation");
    }

    // next should be param_list
    let params = build_param_list(next, lines, depth)?;

    // body
    let body = build_value(
        inner.next().expect("grammar guarantees fn_form has body"),
        lines,
        depth,
    )?;

    Ok(Spanned::new(
        Expr::Fn {
            return_ann,
            params,
            body: Box::new(body),
            desugared: false,
        },
        span,
    ))
}

fn build_param_list(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Vec<Spanned<Param>>, ParseError> {
    let mut params = Vec::new();
    let mut saw_variadic: Option<Span> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::param => {
                if let Some(ref variadic_span) = saw_variadic {
                    return Err(ParseError {
                        message: format!(
                            "parameter after variadic parameter (variadic at {})",
                            variadic_span
                        ),
                        span: Some(make_span(&child, lines)),
                    });
                }
                let p_span = make_span(&child, lines);
                let mut p_inner = child.into_inner();
                let name_pair = p_inner
                    .next()
                    .expect("grammar guarantees param has param_name");
                let name = name_pair.as_str().to_string();

                let annotation = if let Some(ann_pair) = p_inner.next() {
                    // param_annotation = ${ "@" ~ annotation_value }
                    let ann_inner = ann_pair
                        .into_inner()
                        .next()
                        .expect("grammar guarantees param_annotation has annotation_value");
                    Some(build_annotation_value(ann_inner, lines, depth)?)
                } else {
                    None
                };

                params.push(Spanned::new(
                    Param {
                        name,
                        annotation,
                        variadic: false,
                    },
                    p_span,
                ));
            }
            Rule::variadic_param => {
                let p_span = make_span(&child, lines);
                if let Some(ref first_span) = saw_variadic {
                    return Err(ParseError {
                        message: format!("multiple variadic parameters (first at {})", first_span),
                        span: Some(p_span),
                    });
                }
                saw_variadic = Some(p_span);
                // variadic_param is atomic (@{}), so extract name from raw text
                let raw = child.as_str();
                let name = raw
                    .strip_prefix("...")
                    .expect("grammar guarantees variadic_param starts with ...")
                    .to_string();

                params.push(Spanned::new(
                    Param {
                        name,
                        annotation: None,
                        variadic: true,
                    },
                    p_span,
                ));
            }
            other => unreachable!("unexpected rule in param_list: {other:?}"),
        }
    }
    Ok(params)
}

fn build_annotation_value(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Annotation>, ParseError> {
    let span = make_span(&pair, lines);
    let inner = pair
        .into_inner()
        .next()
        .expect("grammar guarantees annotation_value has inner pair");
    match inner.as_rule() {
        Rule::bracket_expr => {
            let bracket_span = make_span(&inner, lines);
            let mut bracket_inner = inner.into_inner().peekable();
            match bracket_inner.peek().map(|p| p.as_rule()) {
                None => Ok(Spanned::new(Annotation::PropertyDict(vec![]), bracket_span)),
                Some(Rule::dict_entries) => {
                    let first = bracket_inner
                        .next()
                        .expect("peek succeeded so next is Some");
                    let entries = build_dict_entries(first, lines, depth)?;
                    // Reject rest entries in property dict annotations with 'type:' key (SPEC 5.6)
                    // Rest entries are allowed in type expressions (e.g., [@[name: String ...] $val])
                    // but not as properties alongside 'type:' (e.g., [x@[type: Int ...]])
                    let has_type_key = entries.iter().any(|e| {
                        e.node
                            .key
                            .as_ref()
                            .map_or(false, |k| matches!(&k.node, Expr::Str(s) if s == "type"))
                    });
                    if has_type_key {
                        if let Some(rest_entry) = entries
                            .iter()
                            .find(|e| matches!(&e.node.value.node, Expr::Rest(_)))
                        {
                            return Err(ParseError {
                                message: "rest entries (...) cannot appear alongside 'type:' in annotation bracket expressions".to_string(),
                                span: Some(rest_entry.span),
                            });
                        }
                    }
                    Ok(Spanned::new(
                        Annotation::PropertyDict(entries),
                        bracket_span,
                    ))
                }
                Some(rule) => Err(ParseError {
                    message: format!(
                        "annotation bracket expression must contain key-value entries, \
                             found {rule:?}"
                    ),
                    span: Some(bracket_span),
                }),
            }
        }
        Rule::annotation_word => Ok(Spanned::new(
            Annotation::Simple(inner.as_str().to_string()),
            span,
        )),
        rule => Err(ParseError {
            message: format!("unexpected annotation value type: {rule:?}"),
            span: Some(span),
        }),
    }
}

fn build_type_alias(
    pair: pest::iterators::Pair<'_, Rule>,
    span: Span,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    // Grammar: type_form = { keyword_type ~ value }
    let mut inner = pair.into_inner();
    let _ = inner.next().expect("grammar guarantees keyword_type");
    let body = build_value(
        inner
            .next()
            .expect("grammar guarantees type_form has body value"),
        lines,
        depth,
    )?;
    Ok(Spanned::new(Expr::TypeAlias(Box::new(body)), span))
}

fn build_dict_entries(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Vec<Spanned<Entry>>, ParseError> {
    let mut entries = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();
    for child in pair.into_inner() {
        if child.as_rule() == Rule::entry {
            let entry = build_entry(child, lines, depth)?;
            if let Some(ref key_expr) = entry.node.key {
                let key_text = key_to_string(&key_expr.node);
                if let Some(key_text) = key_text {
                    if seen_keys.contains(&key_text) {
                        return Err(ParseError {
                            message: format!("duplicate key \"{}\"", key_text),
                            span: Some(key_expr.span),
                        });
                    }
                    seen_keys.insert(key_text);
                }
            }
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Extract a comparable string from a key expression for duplicate detection.
/// Returns None for complex expressions (bracket exprs) where comparison isn't meaningful.
fn key_to_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str(s) => Some(s.clone()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(n) => Some(n.to_string()),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::VarRef(name) => Some(format!("${name}")),
        _ => None,
    }
}

fn build_entry(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Entry>, ParseError> {
    let span = make_span(&pair, lines);
    let inner = pair
        .into_inner()
        .next()
        .expect("grammar guarantees entry has inner pair");

    match inner.as_rule() {
        Rule::keyed_entry => {
            let mut kv = inner.into_inner();
            let key_pair = kv.next().expect("grammar guarantees keyed_entry has key");
            let key_inner = key_pair
                .into_inner()
                .next()
                .expect("grammar guarantees key has inner value");
            let key = build_value(key_inner, lines, depth)?;

            let val_pair = kv.next().expect("grammar guarantees keyed_entry has value");
            let value = build_value(val_pair, lines, depth)?;

            Ok(Spanned::new(
                Entry {
                    key: Some(key),
                    value,
                },
                span,
            ))
        }
        Rule::rest_entry => {
            let raw = inner.as_str();
            let name = raw.strip_prefix("...").expect("rest_entry starts with ...");
            let rest_name = if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            };
            let value = Spanned::new(Expr::Rest(rest_name), span);
            Ok(Spanned::new(Entry { key: None, value }, span))
        }
        Rule::auto_entry => {
            let val_pair = inner
                .into_inner()
                .next()
                .expect("grammar guarantees auto_entry has value");
            let value = build_value(val_pair, lines, depth)?;
            Ok(Spanned::new(Entry { key: None, value }, span))
        }
        rule => Err(ParseError {
            message: format!("unexpected rule in entry: {rule:?}"),
            span: Some(span),
        }),
    }
}

fn build_access_expr(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<Spanned<Expr>, ParseError> {
    let span = make_span(&pair, lines);
    let mut inner = pair.into_inner();

    // First child is var_ref
    let var_pair = inner
        .next()
        .expect("grammar guarantees access_expr has var_ref");
    let var_span = make_span(&var_pair, lines);
    let var_name = var_pair
        .as_str()
        .strip_prefix('$')
        .expect("grammar guarantees var_ref starts with $")
        .to_string();
    let mut current = Spanned::new(Expr::VarRef(var_name), var_span);

    // Remaining children are access_chain elements
    for chain in inner {
        let chain_inner = chain
            .into_inner()
            .next()
            .expect("grammar guarantees access_chain has inner pair");
        match chain_inner.as_rule() {
            Rule::dot_access => {
                let field_pair = chain_inner
                    .into_inner()
                    .next()
                    .expect("grammar guarantees dot_access has field name");
                let field = field_pair.as_str().to_string();
                let new_span = Span {
                    start: current.span.start,
                    end: lines.offset_to_position(field_pair.as_span().end()),
                };
                current = Spanned::new(
                    Expr::DotAccess {
                        expr: Box::new(current),
                        field,
                    },
                    new_span,
                );
            }
            Rule::bracket_access_chain => {
                let chain_end = lines.offset_to_position(chain_inner.as_span().end());
                let bracket_inner = chain_inner
                    .into_inner()
                    .next()
                    .expect("grammar guarantees bracket_access_chain has inner pair");
                // bracket_access_inner: range_expr | value
                let access_inner = bracket_inner
                    .into_inner()
                    .next()
                    .expect("grammar guarantees bracket_access_inner has inner pair");
                match access_inner.as_rule() {
                    Rule::range_expr => {
                        let (start_expr, end_expr) = build_range_expr(access_inner, lines, depth)?;
                        let new_span = Span {
                            start: current.span.start,
                            end: chain_end,
                        };
                        current = Spanned::new(
                            Expr::RangeAccess {
                                expr: Box::new(current),
                                start: start_expr.map(Box::new),
                                end: end_expr.map(Box::new),
                            },
                            new_span,
                        );
                    }
                    _ => {
                        let key = build_value(access_inner, lines, depth)?;
                        let new_span = Span {
                            start: current.span.start,
                            end: chain_end,
                        };
                        current = Spanned::new(
                            Expr::BracketAccess {
                                expr: Box::new(current),
                                key: Box::new(key),
                            },
                            new_span,
                        );
                    }
                }
            }
            rule => {
                return Err(ParseError {
                    message: format!("unexpected rule in access_chain: {rule:?}"),
                    span: Some(span),
                });
            }
        }
    }

    Ok(current)
}

type RangePair = (Option<Spanned<Expr>>, Option<Spanned<Expr>>);

fn build_range_expr(
    pair: pest::iterators::Pair<'_, Rule>,
    lines: &LineTable,
    depth: usize,
) -> Result<RangePair, ParseError> {
    // Grammar: range_expr = { range_value? ~ ".." ~ range_value? }
    // Determine the absolute offset of ".." by finding it in the pair's raw text.
    let pair_start = pair.as_span().start();
    let raw = pair.as_str();
    let span = make_span(&pair, lines);
    let dot_dot_offset = pair_start
        + raw.find("..").ok_or_else(|| ParseError {
            message: "range_expr must contain '..'".to_string(),
            span: Some(span),
        })?;

    let mut start = None;
    let mut end = None;
    for child in pair.into_inner() {
        if child.as_rule() == Rule::range_value {
            let child_start = child.as_span().start();
            let val_inner = child
                .into_inner()
                .next()
                .expect("grammar guarantees range_value has inner value");
            let val = build_value(val_inner, lines, depth)?;
            if child_start < dot_dot_offset {
                start = Some(val);
            } else {
                end = Some(val);
            }
        }
    }
    Ok((start, end))
}

fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                // unreachable: grammar only accepts \n, \t, \r, \\, \"
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> Spanned<Expr> {
        parse_expression(input).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    fn parse_err(input: &str) -> ParseError {
        parse_expression(input).unwrap_err()
    }

    #[test]
    fn test_int_literal() {
        let ast = parse_ok("[42]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(42)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_negative_int() {
        let ast = parse_ok("[-1]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(-1)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_float_literal() {
        let ast = parse_ok("[3.14]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Float(f) => assert!((*f - 3.14).abs() < f64::EPSILON),
                    other => panic!("expected Float, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_negative_float() {
        let ast = parse_ok("[-3.14]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Float(f) => assert!((*f + 3.14).abs() < f64::EPSILON),
                    other => panic!("expected Float, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bool_literals() {
        let ast = parse_ok("[true false]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(&entries[0].node.value.node, Expr::Bool(true)));
                assert!(matches!(&entries[1].node.value.node, Expr::Bool(false)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bool_prefix_is_bare_word() {
        let ast = parse_ok("[truename falsehood]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "truename"));
                assert!(
                    matches!(&entries[1].node.value.node, Expr::Str(ref s) if s == "falsehood")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_quoted_string() {
        let ast = parse_ok(r#"["hello world"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "hello world")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_quoted_overrides_bool() {
        let ast = parse_ok(r#"["true"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "true"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref() {
        let ast = parse_ok("[$name]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "name"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_with_question_mark() {
        let ast = parse_ok("[$has?]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "has?"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word() {
        let ast = parse_ok("[hello]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "hello"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_with_dots() {
        let ast = parse_ok("[some.file.txt]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "some.file.txt")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_dict() {
        let ast = parse_ok("[]");
        assert!(matches!(&ast.node, Expr::Dict(entries) if entries.is_empty()));
    }

    #[test]
    fn test_simple_dict() {
        let ast = parse_ok("[name: Alice  age: 30]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                // name: Alice
                assert!(entries[0].node.key.is_some());
                assert!(
                    matches!(&entries[0].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "name")
                );
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "Alice"));
                // age: 30
                assert!(
                    matches!(&entries[1].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "age")
                );
                assert!(matches!(&entries[1].node.value.node, Expr::Int(30)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_list() {
        let ast = parse_ok("[a b c]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(entries[0].node.key.is_none());
                assert!(entries[1].node.key.is_none());
                assert!(entries[2].node.key.is_none());
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_nested_dict() {
        let ast = parse_ok("[db: [host: localhost  port: 5432]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Dict(inner) => {
                        assert_eq!(inner.len(), 2);
                    }
                    other => panic!("expected nested Dict, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call() {
        let ast = parse_ok("[call $f $x $y]");
        match &ast.node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                assert!(matches!(&func.node, Expr::VarRef(ref s) if s == "f"));
                assert_eq!(args.len(), 2);
                assert!(named_args.is_empty());
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_with_named_args() {
        let ast = parse_ok("[call $fetch $url timeout: 60]");
        match &ast.node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                assert!(matches!(&func.node, Expr::VarRef(ref s) if s == "fetch"));
                assert_eq!(args.len(), 1);
                assert_eq!(named_args.len(), 1);
                assert_eq!(named_args[0].node.name, "timeout");
                assert!(matches!(&named_args[0].node.value.node, Expr::Int(60)));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_colon_is_dict() {
        // call: is a key, not a keyword
        let ast = parse_ok("[call: something]");
        assert!(matches!(&ast.node, Expr::Dict(_)));
    }

    #[test]
    fn test_dollar_call_is_dict() {
        // $call is a var ref, not the keyword
        let ast = parse_ok("[$call $x]");
        assert!(matches!(&ast.node, Expr::Dict(_)));
    }

    #[test]
    fn test_fn_simple() {
        let ast = parse_ok("[fn [x] $x]");
        match &ast.node {
            Expr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                assert!(return_ann.is_none());
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(matches!(&body.node, Expr::VarRef(ref s) if s == "x"));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_with_annotations() {
        let ast = parse_ok("[fn@Number [x@Number y@Number] [call $+ $x $y]]");
        match &ast.node {
            Expr::Fn {
                return_ann, params, ..
            } => {
                assert!(
                    matches!(&return_ann.as_ref().unwrap().node, Annotation::Simple(ref s) if s == "Number")
                );
                assert_eq!(params.len(), 2);
                assert!(params[0].node.annotation.is_some());
                assert!(params[1].node.annotation.is_some());
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_with_property_dict_annotation() {
        let ast = parse_ok("[fn [timeout@[type: Number  default: 30]] $timeout]");
        match &ast.node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                match &params[0].node.annotation.as_ref().unwrap().node {
                    Annotation::PropertyDict(entries) => {
                        assert_eq!(entries.len(), 2);
                    }
                    other => panic!("expected PropertyDict, got {other:?}"),
                }
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_variadic() {
        let ast = parse_ok("[fn [f ...args] [call $map $f $args]]");
        match &ast.node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 2);
                assert!(!params[0].node.variadic);
                assert!(params[1].node.variadic);
                assert_eq!(params[1].node.name, "args");
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias() {
        let ast = parse_ok("[type [a b c]]");
        match &ast.node {
            Expr::TypeAlias(inner) => match &inner.node {
                Expr::Dict(entries) => {
                    assert_eq!(entries.len(), 3);
                    // Auto-indexed entries: keys are None, values are bare words
                    for entry in entries {
                        assert!(entry.node.key.is_none(), "expected auto-indexed entry");
                    }
                    assert!(matches!(&entries[0].node.value.node, Expr::Str(s) if s == "a"));
                    assert!(matches!(&entries[1].node.value.node, Expr::Str(s) if s == "b"));
                    assert!(matches!(&entries[2].node.value.node, Expr::Str(s) if s == "c"));
                }
                other => panic!("expected Dict inside TypeAlias, got {other:?}"),
            },
            other => panic!("expected TypeAlias, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access() {
        let ast = parse_ok("[$person.name]");
        match &ast.node {
            Expr::Dict(entries) => match &entries[0].node.value.node {
                Expr::DotAccess { expr, field } => {
                    assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "person"));
                    assert_eq!(field, "name");
                }
                other => panic!("expected DotAccess, got {other:?}"),
            },
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access() {
        let ast = parse_ok("[$data[0]]");
        match &ast.node {
            Expr::Dict(entries) => match &entries[0].node.value.node {
                Expr::BracketAccess { expr, key } => {
                    assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                    assert!(matches!(&key.node, Expr::Int(0)));
                }
                other => panic!("expected BracketAccess, got {other:?}"),
            },
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_chained_access() {
        let ast = parse_ok("[$config.services[0].host]");
        match &ast.node {
            Expr::Dict(entries) => {
                // Should be DotAccess(BracketAccess(DotAccess(VarRef, "services"), 0), "host")
                match &entries[0].node.value.node {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(field, "host");
                        match &expr.node {
                            Expr::BracketAccess { expr: inner, key } => {
                                assert!(matches!(&key.node, Expr::Int(0)));
                                match &inner.node {
                                    Expr::DotAccess {
                                        expr: var,
                                        field: f2,
                                    } => {
                                        assert!(
                                            matches!(&var.node, Expr::VarRef(ref s) if s == "config")
                                        );
                                        assert_eq!(f2, "services");
                                    }
                                    other => panic!("expected inner DotAccess, got {other:?}"),
                                }
                            }
                            other => panic!("expected BracketAccess, got {other:?}"),
                        }
                    }
                    other => panic!("expected outer DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_space_prevents_access() {
        // $a .b should be two separate entries, not dot access
        let ast = parse_ok("[$a .b]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "a"));
                assert!(matches!(&entries[1].node.value.node, Expr::Str(ref s) if s == ".b"));
            }
            other => panic!("expected Dict with 2 entries, got {other:?}"),
        }
    }

    #[test]
    fn test_type_assert_simple() {
        let ast = parse_ok("[@Number $expr]");
        match &ast.node {
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                assert!(matches!(&annotation.node, Annotation::Simple(ref s) if s == "Number"));
                assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "expr"));
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_type_assert_with_fallback() {
        let ast = parse_ok("[@[type: Number  default: 0] $x]");
        match &ast.node {
            Expr::TypeAssert { annotation, .. } => {
                assert!(matches!(&annotation.node, Annotation::PropertyDict(_)));
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_comments_ignored() {
        let ast = parse_ok(
            "[
            # This is a comment
            name: Alice  # inline comment
            age: 30
        ]",
        );
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_pipeline_example() {
        let ast = parse_ok(
            "[call $-> $data
            [call $filter [call $> $_.age 30] $_]
            [call $map $_.name $_]
            $sort]",
        );
        match &ast.node {
            Expr::Call { func, args, .. } => {
                assert!(matches!(&func.node, Expr::VarRef(ref s) if s == "->"));
                assert_eq!(args.len(), 4);
                assert!(matches!(&args[1].node, Expr::Call { .. }));
                assert!(matches!(&args[2].node, Expr::Call { .. }));
                assert!(matches!(&args[3].node, Expr::VarRef(ref s) if s == "sort"));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_full_config_example() {
        let input = r#"[
            base: [timeout: 30  retries: 3]
            dev:  [call $merge $base [env: dev]]
            prod: [call $merge $base [env: prod  timeout: 60]]
        ]"#;
        let ast = parse_ok(input);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                // base is a Dict
                assert!(matches!(&entries[0].node.value.node, Expr::Dict(_)));
                // dev and prod are Call
                assert!(matches!(&entries[1].node.value.node, Expr::Call { .. }));
                assert!(matches!(&entries[2].node.value.node, Expr::Call { .. }));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_function_definition_as_entry() {
        let ast = parse_ok("[double: [fn@Number [x@Number] [call $* $x 2]]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Fn { .. }));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_error_span_on_unmatched_bracket() {
        let err = parse_err("[hello");
        // pest grammar errors have no span (they come from LltParser::parse)
        // but the message should describe what was expected
        assert!(err.span.is_none());
        assert!(!err.message.is_empty());
    }

    #[test]
    fn test_error_extra_closing_bracket() {
        let err = parse_err("[hello]]");
        assert!(
            err.message.contains("expected EOI"),
            "expected 'expected EOI' in: {}",
            err.message
        );
    }

    #[test]
    fn test_error_unexpected_colon_at_top_level() {
        let err = parse_err(": value");
        assert!(
            err.message.contains("expected file"),
            "expected 'expected file' in: {}",
            err.message
        );
    }

    #[test]
    fn test_error_missing_value_after_colon() {
        let err = parse_err("[key:]");
        assert!(
            err.message.contains("expected value"),
            "expected 'expected value' in: {}",
            err.message
        );
    }

    #[test]
    fn test_error_integer_overflow() {
        // i64::MAX + 1 = 9223372036854775808
        let err = parse_err("[99999999999999999999]");
        assert!(
            err.message.contains("invalid integer"),
            "expected 'invalid integer' in: {}",
            err.message
        );
        assert!(
            err.span.is_some(),
            "integer overflow error should have a span"
        );
    }

    #[test]
    fn test_error_integer_underflow() {
        // Very large negative number
        let err = parse_err("[-99999999999999999999]");
        assert!(
            err.message.contains("invalid integer"),
            "expected 'invalid integer' in: {}",
            err.message
        );
        assert!(err.span.is_some());
    }

    #[test]
    fn test_error_span_position_correctness() {
        // Integer overflow at a known position inside a dict
        let err = parse_err("[a: 99999999999999999999]");
        assert!(err.span.is_some());
        let span = err.span.unwrap();
        // The int literal starts at column 5 (1-indexed): "[a: " = 4 chars, then the number
        assert_eq!(span.start.line, 1);
        assert_eq!(span.start.column, 5);
    }

    #[test]
    fn test_error_span_multiline_position() {
        // Put the overflow on line 2
        let err = parse_err("[a: 1\nb: 99999999999999999999]");
        assert!(err.span.is_some());
        let span = err.span.unwrap();
        assert_eq!(span.start.line, 2);
        assert_eq!(span.start.column, 4); // "b: " = 3 chars, number starts at col 4
    }

    #[test]
    fn test_error_depth_limit() {
        // Verify that moderate nesting works fine (5 levels)
        let ast = parse_ok("[[[[[42]]]]]");
        match ast.node {
            Expr::Dict(_) => {}
            other => panic!("expected Dict at 5-deep nesting, got: {other:?}"),
        }

        // Verify the depth limit triggers by calling build_value directly with
        // a high starting depth. This avoids constructing 256+ nested brackets
        // which is too slow for pest's PEG parser.
        let input = "42";
        let pairs = LltParser::parse(Rule::value, input).unwrap();
        let lines = LineTable::new(input);
        let pair = pairs.into_iter().next().unwrap();

        // At depth=MAX_PARSE_DEPTH, build_value rejects immediately.
        let err = build_value(pair, &lines, MAX_PARSE_DEPTH).unwrap_err();
        assert!(err.message.contains("maximum nesting depth exceeded"));
        assert!(err.span.is_some());

        // At depth=MAX_PARSE_DEPTH-1, parsing should succeed (leaf value, no further nesting).
        let pairs2 = LltParser::parse(Rule::value, input).unwrap();
        let pair2 = pairs2.into_iter().next().unwrap();
        assert!(build_value(pair2, &lines, MAX_PARSE_DEPTH - 1).is_ok());
    }

    #[test]
    fn test_error_depth_limit_message_format() {
        // Verify ParseError Display formatting with and without span
        let err_with_span = ParseError {
            message: format!("maximum nesting depth exceeded ({MAX_PARSE_DEPTH})"),
            span: Some(Span::new(
                Position {
                    offset: 0,
                    line: 1,
                    column: 1,
                },
                Position {
                    offset: 5,
                    line: 1,
                    column: 6,
                },
            )),
        };
        let display = format!("{err_with_span}");
        assert!(display.contains("maximum nesting depth exceeded"));
        assert!(display.contains("1:1"));
    }

    #[test]
    fn test_float_inf() {
        // f64 can parse "inf" but our grammar only accepts digit sequences,
        // so "inf" should parse as a bare word, not a float
        let ast = parse_ok("[inf]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "inf"));
            }
            other => panic!("expected Dict with bare word, got {other:?}"),
        }
    }

    #[test]
    fn test_float_nan() {
        // "nan" should be a bare word, not a float
        let ast = parse_ok("[nan]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "nan"));
            }
            other => panic!("expected Dict with bare word, got {other:?}"),
        }
    }

    #[test]
    fn test_float_very_large() {
        // A very large float that can be parsed by f64 (within range)
        let ast = parse_ok("[1.7976931348623157]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Float(_)));
            }
            other => panic!("expected Dict with Float, got {other:?}"),
        }
    }

    #[test]
    fn test_float_very_small() {
        let ast = parse_ok("[0.000000001]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Float(f) => assert!(*f > 0.0 && *f < 0.001),
                    other => panic!("expected Float, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_float_zero() {
        let ast = parse_ok("[0.0]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Float(f) => assert!((*f).abs() < f64::EPSILON),
                    other => panic!("expected Float(0.0), got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_semicolon_separated_entries() {
        let ast = parse_ok("[a: 1; b: 2; c: 3]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(
                    matches!(&entries[0].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "a")
                );
                assert!(matches!(&entries[0].node.value.node, Expr::Int(1)));
                assert!(
                    matches!(&entries[1].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "b")
                );
                assert!(matches!(&entries[1].node.value.node, Expr::Int(2)));
                assert!(
                    matches!(&entries[2].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "c")
                );
                assert!(matches!(&entries[2].node.value.node, Expr::Int(3)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_semicolon_with_auto_entries() {
        let ast = parse_ok("[a; b; c]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(entries.iter().all(|e| e.node.key.is_none()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_semicolons_and_whitespace() {
        let ast = parse_ok("[a: 1; b: 2  c: 3]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_trailing_semicolon() {
        let ast = parse_ok("[a: 1; b: 2;]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_key_nested_dict() {
        // A bracket expression as a key: [bracket key]: value
        let ast = parse_ok("[[computed key]: value]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().unwrap();
                match &key.node {
                    Expr::Dict(inner) => {
                        assert_eq!(inner.len(), 2);
                        assert!(
                            matches!(&inner[0].node.value.node, Expr::Str(ref s) if s == "computed")
                        );
                        assert!(
                            matches!(&inner[1].node.value.node, Expr::Str(ref s) if s == "key")
                        );
                    }
                    other => panic!("expected Dict as key, got {other:?}"),
                }
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "value"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_as_key() {
        let ast = parse_ok("[$key: value]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().unwrap();
                assert!(matches!(&key.node, Expr::VarRef(ref s) if s == "key"));
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "value"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_newline() {
        let ast = parse_ok(r#"["hello\nworld"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "hello\nworld")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_tab() {
        let ast = parse_ok(r#"["col1\tcol2"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "col1\tcol2")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_carriage_return() {
        let ast = parse_ok(r#"["line\r\nend"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "line\r\nend")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_backslash() {
        let ast = parse_ok(r#"["path\\to\\file"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "path\\to\\file")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_quote() {
        let ast = parse_ok(r#"["say \"hello\""]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "say \"hello\"")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_all_sequences_combined() {
        let ast = parse_ok(r#"["a\nb\tc\r\\d\"e"]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Str(s) => assert_eq!(s, "a\nb\tc\r\\d\"e"),
                    other => panic!("expected Str, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_string() {
        let ast = parse_ok(r#"[""]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s.is_empty()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_unicode() {
        let ast = parse_ok("[caf\u{00e9}]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "caf\u{00e9}")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_cjk() {
        let ast = parse_ok("[\u{4f60}\u{597d}]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "\u{4f60}\u{597d}")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_operator_chars() {
        // Bare words can contain >, <, =, +, * per bare_word_char
        let ast = parse_ok("[>=]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == ">="));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_angle_brackets() {
        let ast = parse_ok("[<=>]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "<=>"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_tilde_and_caret() {
        let ast = parse_ok("[~thing ^other]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "~thing"));
                assert!(matches!(&entries[1].node.value.node, Expr::Str(ref s) if s == "^other"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_word_with_at_sign() {
        // @ is always structural -- name@domain parses as Annotated
        let ast = parse_ok("[name@domain]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::Annotated { name, annotation } => {
                        assert_eq!(name, "name");
                        assert!(
                            matches!(&annotation.node, Annotation::Simple(ref s) if s == "domain")
                        );
                    }
                    other => panic!("expected Annotated, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_int_as_dict_key() {
        let ast = parse_ok("[0: zero  1: one  2: two]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(matches!(
                    &entries[0].node.key.as_ref().unwrap().node,
                    Expr::Int(0)
                ));
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "zero"));
                assert!(matches!(
                    &entries[1].node.key.as_ref().unwrap().node,
                    Expr::Int(1)
                ));
                assert!(matches!(
                    &entries[2].node.key.as_ref().unwrap().node,
                    Expr::Int(2)
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_float_as_dict_key() {
        let ast = parse_ok("[3.14: pi]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Float(f) => assert!((*f - 3.14).abs() < f64::EPSILON),
                    other => panic!("expected Float key, got {other:?}"),
                }
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "pi"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bool_as_dict_key() {
        let ast = parse_ok("[true: yes  false: no]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(
                    &entries[0].node.key.as_ref().unwrap().node,
                    Expr::Bool(true)
                ));
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "yes"));
                assert!(matches!(
                    &entries[1].node.key.as_ref().unwrap().node,
                    Expr::Bool(false)
                ));
                assert!(matches!(&entries[1].node.value.node, Expr::Str(ref s) if s == "no"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_annotation_dict_with_default() {
        let ast = parse_ok("[fn@[type: Number  default: 0] [x] $x]");
        match &ast.node {
            Expr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                let ann = return_ann.as_ref().unwrap();
                match &ann.node {
                    Annotation::PropertyDict(entries) => {
                        assert_eq!(entries.len(), 2);
                        assert!(
                            matches!(&entries[0].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "type")
                        );
                        assert!(
                            matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "Number")
                        );
                        assert!(
                            matches!(&entries[1].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "default")
                        );
                        assert!(matches!(&entries[1].node.value.node, Expr::Int(0)));
                    }
                    other => panic!("expected PropertyDict annotation, got {other:?}"),
                }
                assert_eq!(params.len(), 1);
                assert!(matches!(&body.node, Expr::VarRef(ref s) if s == "x"));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_param_annotation_simple_and_dict_mixed() {
        let ast = parse_ok("[fn [x@Number  y@[type: String  default: hello]] $x]");
        match &ast.node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 2);
                // x has simple annotation
                match &params[0].node.annotation.as_ref().unwrap().node {
                    Annotation::Simple(s) => assert_eq!(s, "Number"),
                    other => panic!("expected Simple annotation on x, got {other:?}"),
                }
                // y has property dict annotation
                match &params[1].node.annotation.as_ref().unwrap().node {
                    Annotation::PropertyDict(entries) => {
                        assert_eq!(entries.len(), 2);
                    }
                    other => panic!("expected PropertyDict annotation on y, got {other:?}"),
                }
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_empty_param_list() {
        let ast = parse_ok("[fn [] 42]");
        match &ast.node {
            Expr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                assert!(return_ann.is_none());
                assert!(params.is_empty());
                assert!(matches!(&body.node, Expr::Int(42)));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_empty_params_with_annotation() {
        let ast = parse_ok("[fn@Number [] 0]");
        match &ast.node {
            Expr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                assert!(
                    matches!(&return_ann.as_ref().unwrap().node, Annotation::Simple(ref s) if s == "Number")
                );
                assert!(params.is_empty());
                assert!(matches!(&body.node, Expr::Int(0)));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_range_both_bounds() {
        let ast = parse_ok("[$data[2..5]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        let s = start.as_ref().expect("start should be Some");
                        assert!(matches!(&s.node, Expr::Int(2)));
                        let e = end.as_ref().expect("end should be Some");
                        assert!(matches!(&e.node, Expr::Int(5)));
                    }
                    other => panic!("expected RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_no_start() {
        let ast = parse_ok("[$data[..3]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        assert!(start.is_none(), "start should be None for [..3]");
                        let e = end.as_ref().expect("end should be Some");
                        assert!(matches!(&e.node, Expr::Int(3)));
                    }
                    other => panic!("expected RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_no_end() {
        let ast = parse_ok("[$data[2..]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        let s = start.as_ref().expect("start should be Some");
                        assert!(matches!(&s.node, Expr::Int(2)));
                        assert!(end.is_none(), "end should be None for [2..]");
                    }
                    other => panic!("expected RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_unbounded() {
        let ast = parse_ok("[$data[..]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        assert!(start.is_none(), "start should be None for [..]");
                        assert!(end.is_none(), "end should be None for [..]");
                    }
                    other => panic!("expected RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_with_var_refs() {
        let ast = parse_ok("[$data[$start..$end]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        let s = start.as_ref().expect("start should be Some");
                        assert!(matches!(&s.node, Expr::VarRef(ref v) if v == "start"));
                        let e = end.as_ref().expect("end should be Some");
                        assert!(matches!(&e.node, Expr::VarRef(ref v) if v == "end"));
                    }
                    other => panic!("expected RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_chained_with_dot_access() {
        let ast = parse_ok("[$items[1..3].name]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(field, "name");
                        assert!(matches!(&expr.node, Expr::RangeAccess { .. }));
                    }
                    other => panic!("expected DotAccess wrapping RangeAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_greater_than() {
        let ast = parse_ok("[$>]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == ">"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_less_equal() {
        let ast = parse_ok("[$<=]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "<="));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_arrow() {
        let ast = parse_ok("[$->]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "->"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_plus() {
        let ast = parse_ok("[$+]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "+"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_star() {
        let ast = parse_ok("[$*]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "*"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_slash() {
        let ast = parse_ok("[$/]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "/"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref_operator_equals() {
        let ast = parse_ok("[$=]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::VarRef(ref s) if s == "="));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_span_simple_int() {
        let ast = parse_ok("[42]");
        // The outer bracket_expr spans the full input
        assert_eq!(ast.span.start.offset, 0);
        assert_eq!(ast.span.end.offset, 4);
        assert_eq!(ast.span.start.line, 1);
        assert_eq!(ast.span.start.column, 1);
    }

    #[test]
    fn test_span_multiline() {
        let ast = parse_ok("[a: 1\nb: 2]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                // Second entry starts on line 2
                let second_entry_span = &entries[1].span;
                assert_eq!(second_entry_span.start.line, 2);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_error_bare_dollar() {
        // $ alone is not a valid var_ref (needs var_ident after it)
        let err = parse_err("[$]");
        assert!(
            err.message
                .contains("expected special_form, type_assert_body, or entry"),
            "expected 'expected special_form, type_assert_body, or entry' in: {}",
            err.message
        );
    }

    #[test]
    fn test_whitespace_only_input_is_empty_dict() {
        // Empty document that's just whitespace is valid (empty dict)
        let ast = parse_ok("   ");
        assert!(matches!(&ast.node, Expr::Dict(entries) if entries.is_empty()));
    }

    #[test]
    fn test_error_double_colon() {
        let err = parse_err("[a:: b]");
        assert!(
            err.message.contains("expected value"),
            "expected 'expected value' in: {}",
            err.message
        );
    }

    #[test]
    fn test_error_display_without_span() {
        let err = ParseError {
            message: "test error".to_string(),
            span: None,
        };
        let display = format!("{err}");
        assert_eq!(display, "test error");
    }

    #[test]
    fn test_error_display_with_span() {
        let err = ParseError {
            message: "test error".to_string(),
            span: Some(Span {
                start: Position {
                    offset: 5,
                    line: 2,
                    column: 3,
                },
                end: Position {
                    offset: 10,
                    line: 2,
                    column: 8,
                },
            }),
        };
        let display = format!("{err}");
        assert_eq!(display, "2:3: test error");
    }

    #[test]
    fn test_single_document_single_expression() {
        let file = parse("[x: 1]").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        assert_eq!(file.node.documents[0].node.expressions.len(), 1);
    }

    #[test]
    fn test_single_document_multiple_expressions() {
        let file = parse("[x: 1]\n\n[y: 2]").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        assert_eq!(file.node.documents[0].node.expressions.len(), 2);
    }

    #[test]
    fn test_two_documents() {
        let file = parse("[x: 1]\n---\n[y: 2]").unwrap();
        assert_eq!(file.node.documents.len(), 2);
        assert_eq!(file.node.documents[0].node.expressions.len(), 1);
        assert_eq!(file.node.documents[1].node.expressions.len(), 1);
    }

    #[test]
    fn test_three_documents() {
        let file = parse("[a: 1]\n---\n[b: 2]\n---\n[c: 3]").unwrap();
        assert_eq!(file.node.documents.len(), 3);
    }

    #[test]
    fn test_document_separator_not_bare_word() {
        // ---- (four hyphens) is a bare word, not a separator
        let file = parse("[x: ----]").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        let expr = &file.node.documents[0].node.expressions[0];
        match &expr.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Str(s) if s == "----"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_file() {
        let file = parse("").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        assert_eq!(file.node.documents[0].node.expressions.len(), 0);
    }

    #[test]
    fn test_whitespace_only_file() {
        let file = parse("   \n\n   ").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        assert_eq!(file.node.documents[0].node.expressions.len(), 0);
    }

    #[test]
    fn test_multi_expression_with_call() {
        // Simulates include-as-expression followed by a dict
        let file = parse("[call $include \"lib.llt\"]\n\n[result: 42]").unwrap();
        assert_eq!(file.node.documents.len(), 1);
        let doc = &file.node.documents[0];
        assert_eq!(doc.node.expressions.len(), 2);
        assert!(matches!(&doc.node.expressions[0].node, Expr::Call { .. }));
        assert!(matches!(&doc.node.expressions[1].node, Expr::Dict(_)));
    }

    #[test]
    fn test_document_separator_with_whitespace() {
        let file = parse("[x: 1]\n\n---\n\n[y: 2]").unwrap();
        assert_eq!(file.node.documents.len(), 2);
    }

    #[test]
    fn test_file_display() {
        let file = parse("[x: 1]\n---\n[y: 2]").unwrap();
        let display = format!("{}", file.node);
        assert!(display.contains("---"));
    }

    #[test]
    fn test_document_with_pipeline_variable() {
        let file = parse("[data: 1]\n---\n[result: $$.data]").unwrap();
        assert_eq!(file.node.documents.len(), 2);
        let expr = &file.node.documents[1].node.expressions[0];
        match &expr.node {
            Expr::Dict(entries) => {
                assert!(matches!(
                    &entries[0].node.value.node,
                    Expr::DotAccess { expr, field }
                    if matches!(&expr.node, Expr::VarRef(n) if n == "$") && field == "data"
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_positional_and_named_in_dict() {
        let ast = parse_ok("[a key: val b]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(entries[0].node.key.is_none());
                assert!(entries[1].node.key.is_some());
                assert!(entries[2].node.key.is_none());
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_positional_and_named_in_call() {
        let ast = parse_ok("[call $f key: 1 $x]");
        match &ast.node {
            Expr::Call {
                args, named_args, ..
            } => {
                assert_eq!(args.len(), 1);
                assert_eq!(named_args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_error_duplicate_key() {
        let err = parse_err("[name: Alice  name: Bob]");
        assert!(
            err.message.contains("duplicate key"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_duplicate_auto_indexed_values_ok() {
        // Auto-indexed entries with same value are NOT duplicates
        let ast = parse_ok("[a b a]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_error_duplicate_int_key() {
        let err = parse_err("[0: zero  0: nil]");
        assert!(
            err.message.contains("duplicate key"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_error_multiple_variadics() {
        let err = parse_err("[fn [...a ...b] $a]");
        assert!(
            err.message.contains("multiple variadic"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_error_param_after_variadic() {
        let err = parse_err("[fn [...rest x] $x]");
        assert!(
            err.message.contains("parameter after variadic"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_variadic_last_ok() {
        let ast = parse_ok("[fn [x y ...rest] $x]");
        match &ast.node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 3);
                assert!(!params[0].node.variadic);
                assert!(!params[1].node.variadic);
                assert!(params[2].node.variadic);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_line_table_single_line() {
        let lt = LineTable::new("hello");
        let pos = lt.offset_to_position(0);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.column, 1);
        let pos = lt.offset_to_position(4);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.column, 5);
    }

    #[test]
    fn test_line_table_multi_line() {
        let lt = LineTable::new("ab\ncd\nef");
        // 'a' = offset 0 -> line 1, col 1
        assert_eq!(
            lt.offset_to_position(0),
            Position {
                offset: 0,
                line: 1,
                column: 1
            }
        );
        // 'c' = offset 3 -> line 2, col 1
        assert_eq!(
            lt.offset_to_position(3),
            Position {
                offset: 3,
                line: 2,
                column: 1
            }
        );
        // 'e' = offset 6 -> line 3, col 1
        assert_eq!(
            lt.offset_to_position(6),
            Position {
                offset: 6,
                line: 3,
                column: 1
            }
        );
        // 'f' = offset 7 -> line 3, col 2
        assert_eq!(
            lt.offset_to_position(7),
            Position {
                offset: 7,
                line: 3,
                column: 2
            }
        );
    }

    #[test]
    fn test_line_table_offset_on_newline() {
        let lt = LineTable::new("ab\ncd");
        // '\n' = offset 2 -> still on line 1, column 3
        assert_eq!(
            lt.offset_to_position(2),
            Position {
                offset: 2,
                line: 1,
                column: 3
            }
        );
    }

    #[test]
    fn test_line_table_empty() {
        let lt = LineTable::new("");
        let pos = lt.offset_to_position(0);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.column, 1);
    }

    #[test]
    fn test_parse_expression_returns_last() {
        let ast = parse_expression("[a: 1]\n[b: 2]").unwrap();
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(
                    matches!(&entries[0].node.key.as_ref().unwrap().node, Expr::Str(ref s) if s == "b")
                );
                assert!(matches!(&entries[0].node.value.node, Expr::Int(2)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_expression_empty_input() {
        let ast = parse_expression("").unwrap();
        // Empty input should return an empty dict
        assert!(
            matches!(&ast.node, Expr::Dict(entries) if entries.is_empty()),
            "expected empty Dict for empty input, got: {:?}",
            ast.node
        );
    }

    #[test]
    fn test_bracket_access_with_string_key() {
        let ast = parse_ok(r#"[$data["key"]]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::BracketAccess { expr, key } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        assert!(matches!(&key.node, Expr::Str(ref s) if s == "key"));
                    }
                    other => panic!("expected BracketAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access_with_var_ref_key() {
        let ast = parse_ok("[$data[$var]]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.value.node {
                    Expr::BracketAccess { expr, key } => {
                        assert!(matches!(&expr.node, Expr::VarRef(ref s) if s == "data"));
                        assert!(matches!(&key.node, Expr::VarRef(ref s) if s == "var"));
                    }
                    other => panic!("expected BracketAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_quoted_string_as_key() {
        let ast = parse_ok(r#"["my key": value]"#);
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().unwrap();
                assert!(matches!(&key.node, Expr::Str(ref s) if s == "my key"));
                assert!(matches!(&entries[0].node.value.node, Expr::Str(ref s) if s == "value"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // -- Rest entries --

    #[test]
    fn test_rest_entry_anonymous() {
        let ast = parse_ok("[a: 1 ...]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(&entries[1].node.value.node, Expr::Rest(None)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_entry_named() {
        let ast = parse_ok("[a: 1 ...extra]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(
                    matches!(&entries[1].node.value.node, Expr::Rest(Some(ref n)) if n == "extra")
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_entry_only() {
        let ast = parse_ok("[...]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Rest(None)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_entry_after_keyed_entries_allowed() {
        let ast = parse_ok("[name: String  age: Number ...]");
        match &ast.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(entries[0].node.key.is_some());
                assert!(entries[1].node.key.is_some());
                assert!(matches!(&entries[2].node.value.node, Expr::Rest(None)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_in_type_context() {
        let ast = parse_ok("[type [name: String ...]]");
        match &ast.node {
            Expr::TypeAlias(inner) => match &inner.node {
                Expr::Dict(entries) => {
                    assert_eq!(entries.len(), 2);
                    assert!(matches!(&entries[1].node.value.node, Expr::Rest(None)));
                }
                other => panic!("expected Dict, got {other:?}"),
            },
            other => panic!("expected TypeAlias, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_named_in_type_context() {
        let ast = parse_ok("[type [name: String ...rest]]");
        match &ast.node {
            Expr::TypeAlias(inner) => match &inner.node {
                Expr::Dict(entries) => {
                    assert_eq!(entries.len(), 2);
                    assert!(
                        matches!(&entries[1].node.value.node, Expr::Rest(Some(ref n)) if n == "rest")
                    );
                }
                other => panic!("expected Dict, got {other:?}"),
            },
            other => panic!("expected TypeAlias, got {other:?}"),
        }
    }

    #[test]
    fn test_annotation_bracket_special_form_rejected() {
        let result = parse("[fn [x@[call $f $x]] $x]");
        assert!(
            result.is_err(),
            "special form in annotation bracket should be rejected"
        );
    }

    #[test]
    fn test_annotation_bracket_dict_entries_accepted() {
        let result = parse("[fn [x@[type: Number  default: 0]] $x]");
        assert!(
            result.is_ok(),
            "dict entries in annotation bracket should be accepted"
        );
    }

    #[test]
    fn test_annotation_bracket_rest_entry_with_type_key_rejected() {
        let result = parse("[fn [x@[type: Int  ...rest]] $x]");
        assert!(
            result.is_err(),
            "rest entry alongside type: key in annotation bracket should be rejected"
        );
    }

    #[test]
    fn test_annotation_bracket_rest_entry_without_type_key_allowed() {
        let result = parse("[fn [x@[default: 0  ...rest]] $x]");
        assert!(
            result.is_ok(),
            "rest entry without type: key in annotation bracket should be allowed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_type_assert_special_form_rejected() {
        let result = parse("[@[call $f $x] 42]");
        assert!(
            result.is_err(),
            "special form in type assert annotation should be rejected"
        );
    }

    #[test]
    fn test_line_table_unix_endings() {
        let table = LineTable::new("abc\ndef\nghi");
        assert_eq!(
            table.offset_to_position(0),
            Position {
                offset: 0,
                line: 1,
                column: 1
            }
        );
        assert_eq!(
            table.offset_to_position(4),
            Position {
                offset: 4,
                line: 2,
                column: 1
            }
        );
        assert_eq!(
            table.offset_to_position(5),
            Position {
                offset: 5,
                line: 2,
                column: 2
            }
        );
        assert_eq!(
            table.offset_to_position(8),
            Position {
                offset: 8,
                line: 3,
                column: 1
            }
        );
    }

    #[test]
    fn test_line_table_crlf_endings() {
        let table = LineTable::new("abc\r\ndef\r\nghi");
        assert_eq!(
            table.offset_to_position(0),
            Position {
                offset: 0,
                line: 1,
                column: 1
            }
        );
        assert_eq!(
            table.offset_to_position(5),
            Position {
                offset: 5,
                line: 2,
                column: 1
            }
        );
        assert_eq!(
            table.offset_to_position(6),
            Position {
                offset: 6,
                line: 2,
                column: 2
            }
        );
        assert_eq!(
            table.offset_to_position(10),
            Position {
                offset: 10,
                line: 3,
                column: 1
            }
        );
    }

    #[test]
    fn test_parse_crlf_multiline() {
        let input = "[x: 1\r\ny: 2]";
        let file = parse(input).unwrap();
        let doc = &file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        match &doc.expressions[0].node {
            Expr::Dict(entries) => assert_eq!(entries.len(), 2),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_crlf_multi_document() {
        let input = "[x: 1]\r\n---\r\n[y: 2]";
        let file = parse(input).unwrap();
        assert_eq!(file.node.documents.len(), 2);
    }
}
