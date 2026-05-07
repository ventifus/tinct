//! AST-to-dict serialization for quasiquoting, macros, and formatter.
//!
//! Converts AST nodes to tinct `Value::Dict` matching the canonical schema
//! defined in `doc/whatif/ast-schema.md`.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{
    Annotation, Document, DotKey, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned,
};
use crate::error::EvalResult;
use crate::value::{Key, Thunk, Value};

/// Error type for AST dict validation failures during dict-to-AST conversion.
#[derive(Debug, Clone)]
pub struct AstError {
    pub message: String,
    /// Field path for error context, e.g. ["type"] or ["args", "0", "value"].
    /// Empty vector means error at the root level.
    pub field_path: Vec<String>,
}

impl fmt::Display for AstError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field_path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(
                f,
                "{} at field path: {}",
                self.message,
                self.field_path.join(".")
            )
        }
    }
}

impl std::error::Error for AstError {}

/// Options controlling AST-to-dict output.
#[derive(Default, Clone)]
pub struct AstToDictOpts<'a> {
    /// Source text — enables `bare:` flag on string literals.
    /// None → bare is always false (safe default for generated code).
    pub source: Option<&'a str>,
    /// Comment maps from ParseOutput — enables leading-comments, trailing-comment,
    /// and blank-before fields on Entry and Document nodes.
    /// None → no comment fields emitted (compact formatter, quasiquoting).
    pub comments: Option<CommentMaps<'a>>,
}

/// Comment and blank-line metadata from ParseOutput.
#[derive(Clone)]
pub struct CommentMaps<'a> {
    pub leading_comments: &'a std::collections::BTreeMap<usize, Vec<String>>,
    pub trailing_comments: &'a std::collections::BTreeMap<usize, String>,
    pub blank_before: &'a std::collections::BTreeMap<usize, bool>,
}

/// Converts a full File AST to a tinct thunk matching the canonical schema.
///
/// The root dict carries `schema-version: 1` and wraps documents as a list.
pub fn ast_to_dict(
    file: &File,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Rc<Thunk>> {
    let span = file
        .documents
        .first()
        .map(|d| d.span)
        .unwrap_or_else(Span::origin);
    let mut root = IndexMap::new();

    root.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String("file".into()),
            span,
        ))),
    );

    root.insert(
        Key::String("schema-version".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(1), span))),
    );

    // documents: list of document dicts
    let docs = file
        .documents
        .iter()
        .map(|doc| document_to_dict(&doc.node, doc.span, opts, ctx))
        .collect::<EvalResult<Vec<_>>>()?;

    root.insert(
        Key::String("documents".into()),
        list_to_thunk_id(docs, span, ctx)?,
    );

    root.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    Ok(Rc::new(Thunk::new_materialized(Value::Dict(root), span)))
}

/// Converts a single expression to a thunk. Used by quasiquoting.
pub fn ast_to_dict_expr(
    expr: &Spanned<Expr>,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Rc<Thunk>> {
    expr_to_thunk(&expr.node, expr.span, opts, ctx)
}

fn document_to_dict(
    doc: &Document,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String("document".into()),
            span,
        ))),
    );

    // expressions: list of expression dicts
    let exprs = doc
        .expressions
        .iter()
        .map(|e| expr_to_thunk_id(&e.node, e.span, opts, ctx))
        .collect::<EvalResult<Vec<_>>>()?;

    dict.insert(
        Key::String("expressions".into()),
        list_to_thunk_id(exprs, span, ctx)?,
    );

    // name: string or []
    dict.insert(
        Key::String("name".into()),
        match &doc.name {
            Some(s) => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::String(s.clone()),
                span,
            ))),
            None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    // output-type: annotation or []
    dict.insert(
        Key::String("output-type".into()),
        match &doc.output_type {
            Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
            None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    // expects: annotation or []
    dict.insert(
        Key::String("expects".into()),
        match &doc.expects {
            Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
            None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    // leading-comments: absent when None or empty
    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&span.start.offset) {
            if !comments.is_empty() {
                let comment_ids: Vec<ThunkId> = comments
                    .iter()
                    .map(|c| {
                        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String(c.clone()),
                            span,
                        )))
                    })
                    .collect();
                dict.insert(
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids, span, ctx)?,
                );
            }
        }
    }

    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn expr_to_thunk(
    expr: &Expr,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Rc<Thunk>> {
    let id = expr_to_thunk_id(expr, span, opts, ctx)?;
    Ok(ctx.thunk_arena.borrow().get(id).clone())
}

fn expr_to_thunk_id(
    expr: &Expr,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    match expr {
        Expr::Int(n) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("literal".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("int".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(*n), span))),
            );
        }

        Expr::Float(f) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("literal".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("float".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Float(*f), span))),
            );
        }

        Expr::Bool(b) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("literal".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("bool".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Bool(*b), span))),
            );
        }

        Expr::Str(s) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("literal".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("str".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(s.clone()),
                    span,
                ))),
            );

            // bare: true if source text at span start is not a quote
            let bare = opts
                .source
                .map(|src| {
                    let offset = span.start.offset;
                    src.as_bytes()
                        .get(offset)
                        .map(|&b| b != b'"')
                        .unwrap_or(false)
                })
                .unwrap_or(false);

            dict.insert(
                Key::String("bare".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Bool(bare), span))),
            );
        }

        Expr::VarRef { name, .. } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("var".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
        }

        Expr::DotAccess {
            expr: target,
            field,
        } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("dot-access".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("target".into()),
                expr_to_thunk_id(&target.node, target.span, opts, ctx)?,
            );

            // field is either String or Int
            match field {
                DotKey::Ident(s) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String(s.clone()),
                            span,
                        ))),
                    );
                }
                DotKey::Int(n) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(*n), span))),
                    );
                }
            }
        }

        Expr::Pipe { lhs, rhs } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("pipe".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("lhs".into()),
                expr_to_thunk_id(&lhs.node, lhs.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("rhs".into()),
                expr_to_thunk_id(&rhs.node, rhs.span, opts, ctx)?,
            );
        }

        Expr::Sequential(exprs) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("sequential".into()),
                    span,
                ))),
            );

            let expr_ids = exprs
                .iter()
                .map(|e| expr_to_thunk_id(&e.node, e.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("exprs".into()),
                list_to_thunk_id(expr_ids, span, ctx)?,
            );
        }

        Expr::Dict(entries) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("dict".into()),
                    span,
                ))),
            );

            let entry_ids = entries
                .iter()
                .map(|e| entry_to_thunk_id(&e.node, e.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids, span, ctx)?,
            );
        }

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("call".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("fn".into()),
                expr_to_thunk_id(&func.node, func.span, opts, ctx)?,
            );

            // args: list of expression dicts
            let arg_ids = args
                .iter()
                .map(|a| expr_to_thunk_id(&a.node, a.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("args".into()),
                list_to_thunk_id(arg_ids, span, ctx)?,
            );

            // named-args: list of [name: str value: expr] dicts
            let named_arg_ids = named_args
                .iter()
                .map(|na| named_arg_to_thunk_id(&na.node, na.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("named-args".into()),
                list_to_thunk_id(named_arg_ids, span, ctx)?,
            );
            dict.insert(
                Key::String("implied".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::Bool(*implied),
                    span,
                ))),
            );
        }

        Expr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("fn".into()),
                    span,
                ))),
            );

            // params: list of param dicts
            let param_ids = params
                .iter()
                .map(|p| param_to_thunk_id(&p.node, span, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("params".into()),
                list_to_thunk_id(param_ids, span, ctx)?,
            );

            // return-ann: annotation or []
            dict.insert(
                Key::String("return-ann".into()),
                match return_ann {
                    Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
                    None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span,
                    ))),
                },
            );

            dict.insert(
                Key::String("body".into()),
                expr_to_thunk_id(&body.node, body.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("desugared".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::Bool(*desugared),
                    span,
                ))),
            );
        }

        Expr::TypeAlias(inner) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("type-alias".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("type-assert".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span, ctx)?,
            );
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::Annotated { name, annotation } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("annotated".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span, ctx)?,
            );
        }

        Expr::Rest(name_opt) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("rest".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                match name_opt {
                    Some(s) => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                        Value::String(s.clone()),
                        span,
                    ))),
                    None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span,
                    ))),
                },
            );
        }

        Expr::Quote(inner) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("quote".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::Unquote(inner) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("unquote".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::UnquoteSplice(inner) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("unquote-splice".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::DefMacro { name, transformer } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("defmacro".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("transformer".into()),
                expr_to_thunk_id(&transformer.node, transformer.span, opts, ctx)?,
            );
        }

        Expr::Match { scrutinee, arms } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("match".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("scrutinee".into()),
                expr_to_thunk_id(&scrutinee.node, scrutinee.span, opts, ctx)?,
            );
            // Serialize arms as a list
            let arms_thunks: Vec<ThunkId> = arms
                .iter()
                .enumerate()
                .map(|(_i, arm)| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        pattern_to_thunk_id(&arm.pattern.node, arm.pattern.span, ctx)?,
                    );
                    arm_dict.insert(
                        Key::String("body".into()),
                        expr_to_thunk_id(&arm.body.node, arm.body.span, opts, ctx)?,
                    );
                    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                        Value::Dict(arm_dict),
                        arm.pattern.span, // Use arm pattern span for the arm dict
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;
            let arms_dict: IndexMap<Key, ThunkId> = arms_thunks
                .into_iter()
                .enumerate()
                .map(|(i, thunk_id)| (Key::Int(i as i64), thunk_id))
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span,
                ))),
            );
        }

        Expr::Error(error_span) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("error".into()),
                    *error_span,
                ))),
            );
            // Use the error's own span, not the outer span
            dict.insert(
                Key::String("span".into()),
                span_to_thunk_id(*error_span, ctx)?,
            );
            return Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(dict),
                *error_span,
            ))));
        }
    }

    // Add span to every node (unless it's Error which handles its own span)
    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a pattern to a ThunkId containing a dict representation.
fn pattern_to_thunk_id(
    pattern: &crate::ast::Pattern,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    use crate::ast::{LiteralPattern, Pattern};
    let mut dict = IndexMap::new();

    match pattern {
        Pattern::Wildcard => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("wildcard".into()),
                    span,
                ))),
            );
        }
        Pattern::Variable(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("variable".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
        }
        Pattern::TypeTag(tag) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("type_tag".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("tag".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(tag.clone()),
                    span,
                ))),
            );
        }
        Pattern::Pin(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("pin".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
        }
        Pattern::Literal(lit) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("literal".into()),
                    span,
                ))),
            );
            let value = match lit {
                LiteralPattern::Int(n) => Value::Int(*n),
                LiteralPattern::Float(f) => Value::Float(*f),
                LiteralPattern::Bool(b) => Value::Bool(*b),
                LiteralPattern::Str(s) => Value::String(s.clone()),
            };
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(value, span))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn entry_to_thunk_id(
    entry: &Entry,
    entry_span: Span,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let span = entry.value.span;
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String("entry".into()),
            span,
        ))),
    );

    // key: expression or []
    dict.insert(
        Key::String("key".into()),
        match &entry.key {
            Some(k) => expr_to_thunk_id(&k.node, k.span, opts, ctx)?,
            None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    dict.insert(
        Key::String("value".into()),
        expr_to_thunk_id(&entry.value.node, entry.value.span, opts, ctx)?,
    );

    // blank-before: true if there was a blank line before this entry
    // Use entry_span (outer Spanned<Entry> span) for comment lookups — the parser keys
    // comments/blank-before by the first token's offset, which is the key's offset for
    // keyed entries and the value's offset for positional entries.
    let blank_before = opts
        .comments
        .as_ref()
        .and_then(|maps| maps.blank_before.get(&entry_span.start.offset))
        .copied()
        .unwrap_or(false);
    dict.insert(
        Key::String("blank-before".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Bool(blank_before),
            span,
        ))),
    );

    // leading-comments: absent when None or empty
    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&entry_span.start.offset) {
            if !comments.is_empty() {
                let comment_ids: Vec<ThunkId> = comments
                    .iter()
                    .map(|c| {
                        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String(c.clone()),
                            span,
                        )))
                    })
                    .collect();
                dict.insert(
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids, span, ctx)?,
                );
            }
        }

        // trailing-comment: absent when None
        if let Some(comment) = comment_maps.trailing_comments.get(&entry_span.start.offset) {
            dict.insert(
                Key::String("trailing-comment".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(comment.clone()),
                    span,
                ))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn named_arg_to_thunk_id(
    named_arg: &NamedArg,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String(named_arg.name.clone()),
            span,
        ))),
    );
    dict.insert(
        Key::String("value".into()),
        expr_to_thunk_id(&named_arg.value.node, named_arg.value.span, opts, ctx)?,
    );
    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn param_to_thunk_id(
    param: &Param,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String(param.name.clone()),
            span,
        ))),
    );

    // annotation: annotation or []
    dict.insert(
        Key::String("annotation".into()),
        match &param.annotation {
            Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
            None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    dict.insert(
        Key::String("variadic".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Bool(param.variadic),
            span,
        ))),
    );

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn annotation_to_thunk_id(
    ann: &Annotation,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::String("annotation".into()),
            span,
        ))),
    );

    match ann {
        Annotation::Simple(name) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("simple".into()),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String(name.clone()),
                    span,
                ))),
            );
        }
        Annotation::PropertyDict(entries) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                    Value::String("dict".into()),
                    span,
                ))),
            );

            // Convert entries to thunk IDs - these are annotation entries (simpler than regular entries)
            let entry_ids = entries
                .iter()
                .map(|e| {
                    let mut entry_dict = IndexMap::new();
                    entry_dict.insert(
                        Key::String("type".into()),
                        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String("entry".into()),
                            e.span,
                        ))),
                    );

                    // For annotation dicts, keys are always string literals (bare words)
                    let key_id = match &e.node.key {
                        Some(k) => match &k.node {
                            Expr::Str(s) => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                                Value::String(s.clone()),
                                k.span,
                            ))),
                            _ => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                                Value::Dict(IndexMap::new()),
                                k.span,
                            ))),
                        },
                        None => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::Dict(IndexMap::new()),
                            e.span,
                        ))),
                    };

                    entry_dict.insert(Key::String("key".into()), key_id);

                    // Value is also typically a string literal in annotations
                    let value_id = match &e.node.value.node {
                        Expr::Str(s) => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String(s.clone()),
                            e.node.value.span,
                        ))),
                        Expr::Int(n) => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::Int(*n),
                            e.node.value.span,
                        ))),
                        _ => ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                            Value::String(format!("<expr at {}>", e.node.value.span)),
                            e.node.value.span,
                        ))),
                    };

                    entry_dict.insert(Key::String("value".into()), value_id);
                    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
                        Value::Dict(entry_dict),
                        e.span,
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids, span, ctx)?,
            );
        }
    }

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn span_to_thunk_id(span: Span, ctx: &Rc<crate::eval::EvalContext>) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    // start position
    let mut start_dict = IndexMap::new();
    start_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            span,
        ))),
    );
    start_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            span,
        ))),
    );
    start_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            span,
        ))),
    );

    // end position
    let mut end_dict = IndexMap::new();
    end_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            span,
        ))),
    );
    end_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            span,
        ))),
    );
    end_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            span,
        ))),
    );

    dict.insert(
        Key::String("start".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Dict(start_dict),
            span,
        ))),
    );
    dict.insert(
        Key::String("end".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
            Value::Dict(end_dict),
            span,
        ))),
    );

    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a Vec<ThunkId> to a dict-based list (auto-indexed dict with integer keys).
fn list_to_thunk_id(
    items: Vec<ThunkId>,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    for (i, item) in items.into_iter().enumerate() {
        dict.insert(Key::Int(i as i64), item);
    }
    Ok(ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Converts a tinct dict (materialized Value) back to an `Expr` AST node.
///
/// Validates that the dict conforms to the canonical AST schema. Returns
/// `AstError` if validation fails. Unknown fields are ignored (forward-compatible).
///
/// The `ctx` parameter is needed to dereference ThunkIds embedded in the dict structure.
pub fn dict_to_ast(
    val: &Value,
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Spanned<Expr>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "expected Dict".into(),
                field_path: vec![],
            })
        }
    };

    // Extract the type discriminator
    let type_str = get_string_field(dict, "type", &[], ctx)?;

    // Extract span (optional — if absent, use synthetic origin span)
    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    let expr = match type_str.as_str() {
        "literal" => {
            let kind = get_string_field(dict, "kind", &["type"], ctx)?;
            match kind.as_str() {
                "int" => {
                    let value = get_int_field(dict, "value", &["kind"], ctx)?;
                    Expr::Int(value)
                }
                "float" => {
                    let value = get_float_field(dict, "value", &["kind"], ctx)?;
                    Expr::Float(value)
                }
                "bool" => {
                    let value = get_bool_field(dict, "value", &["kind"], ctx)?;
                    Expr::Bool(value)
                }
                "str" => {
                    let value = get_string_field(dict, "value", &["kind"], ctx)?;
                    Expr::Str(value)
                }
                _ => {
                    return Err(AstError {
                        message: format!("unknown literal kind: {}", kind),
                        field_path: vec!["kind".into()],
                    })
                }
            }
        }

        "var" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            Expr::VarRef {
                name,
                resolved: RefCell::new(None),
            }
        }

        "dot-access" => {
            let target_val = get_dict_field(dict, "target", &["type"], ctx)?;
            let target = Box::new(dict_to_ast(&target_val, ctx)?);

            let field_val = get_field(dict, "field", &["type"], ctx)?;
            let field = match field_val {
                Value::String(s) => DotKey::Ident(s),
                Value::Int(n) => DotKey::Int(n),
                _ => {
                    return Err(AstError {
                        message: "field must be String or Int".into(),
                        field_path: vec!["field".into()],
                    })
                }
            };

            Expr::DotAccess {
                expr: target,
                field,
            }
        }

        "pipe" => {
            let lhs_val = get_dict_field(dict, "lhs", &["type"], ctx)?;
            let rhs_val = get_dict_field(dict, "rhs", &["type"], ctx)?;
            Expr::Pipe {
                lhs: Box::new(dict_to_ast(&lhs_val, ctx)?),
                rhs: Box::new(dict_to_ast(&rhs_val, ctx)?),
            }
        }

        "sequential" => {
            let exprs_val = get_dict_field(dict, "exprs", &["type"], ctx)?;
            let exprs_list = extract_list(&exprs_val, &["exprs"], ctx)?;
            let mut exprs = Vec::new();
            for (_i, expr_val) in exprs_list.into_iter().enumerate() {
                let expr = dict_to_ast(&expr_val, ctx)?;
                exprs.push(Rc::new(expr));
            }
            Expr::Sequential(exprs)
        }

        "dict" => {
            let entries_val = get_dict_field(dict, "entries", &["type"], ctx)?;
            let entries_list = extract_list(&entries_val, &["entries"], ctx)?;
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let entry = dict_to_entry(&entry_val, &["entries", &i_str], ctx)?;
                entries.push(entry);
            }
            Expr::Dict(entries)
        }

        "call" => {
            let fn_val = get_dict_field(dict, "fn", &["type"], ctx)?;
            let func = Box::new(dict_to_ast(&fn_val, ctx)?);

            let args_val = get_dict_field(dict, "args", &["type"], ctx)?;
            let args_list = extract_list(&args_val, &["args"], ctx)?;
            let mut args = Vec::new();
            for (_i, arg_val) in args_list.into_iter().enumerate() {
                let arg = dict_to_ast(&arg_val, ctx)?;
                args.push(Rc::new(arg));
            }

            let named_args_val = get_dict_field(dict, "named-args", &["type"], ctx)?;
            let named_args_list = extract_list(&named_args_val, &["named-args"], ctx)?;
            let mut named_args = Vec::new();
            for (i, na_val) in named_args_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let na = dict_to_named_arg(&na_val, &["named-args", &i_str], ctx)?;
                named_args.push(na);
            }

            let implied = get_bool_field(dict, "implied", &["type"], ctx)?;

            Expr::Call {
                func,
                args,
                named_args,
                implied,
            }
        }

        "fn" => {
            let params_val = get_dict_field(dict, "params", &["type"], ctx)?;
            let params_list = extract_list(&params_val, &["params"], ctx)?;
            let mut params = Vec::new();
            for (i, param_val) in params_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let param = dict_to_param(&param_val, &["params", &i_str], ctx)?;
                params.push(param);
            }

            let return_ann = match get_optional_dict_field(dict, "return-ann", ctx)? {
                Some(ann_val) if !is_empty_dict(&ann_val) => {
                    Some(dict_to_annotation(&ann_val, &["return-ann"], ctx)?)
                }
                _ => None,
            };

            let body_val = get_dict_field(dict, "body", &["type"], ctx)?;
            let body = Rc::new(dict_to_ast(&body_val, ctx)?);

            let desugared = get_bool_field(dict, "desugared", &["type"], ctx)?;

            Expr::Fn {
                return_ann,
                params,
                body,
                desugared,
            }
        }

        "type-alias" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::TypeAlias(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "type-assert" => {
            let annotation_val = get_dict_field(dict, "annotation", &["type"], ctx)?;
            let annotation = dict_to_annotation(&annotation_val, &["annotation"], ctx)?;

            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            let expr = Box::new(dict_to_ast(&expr_val, ctx)?);

            Expr::TypeAssert {
                annotation,
                expr,
                resolved_type: RefCell::new(None),
            }
        }

        "annotated" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let annotation_val = get_dict_field(dict, "annotation", &["type"], ctx)?;
            let annotation = dict_to_annotation(&annotation_val, &["annotation"], ctx)?;

            Expr::Annotated { name, annotation }
        }

        "rest" => {
            let name_val = get_field(dict, "name", &["type"], ctx)?;
            let name_opt = match name_val {
                Value::String(s) => Some(s),
                Value::Dict(d) if d.is_empty() => None,
                _ => {
                    return Err(AstError {
                        message: "name must be String or empty Dict".into(),
                        field_path: vec!["name".into()],
                    })
                }
            };
            Expr::Rest(name_opt)
        }

        "quote" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::Quote(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "unquote" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::Unquote(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "unquote-splice" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::UnquoteSplice(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "defmacro" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let transformer_val = get_dict_field(dict, "transformer", &["type"], ctx)?;
            Expr::DefMacro {
                name,
                transformer: Box::new(dict_to_ast(&transformer_val, ctx)?),
            }
        }

        "error" => {
            // Error nodes preserve their span
            let error_span = extract_span(dict, ctx).unwrap_or_else(Span::origin);
            Expr::Error(error_span)
        }

        _ => {
            return Err(AstError {
                message: format!("unknown type discriminator: {}", type_str),
                field_path: vec!["type".into()],
            })
        }
    };

    Ok(Spanned::new(expr, span))
}

// Helper functions for extracting values from dicts with error context

fn get_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let thunk_id = dict.get(&Key::String(key.into())).ok_or_else(|| AstError {
        message: format!("missing required field: {}", key),
        field_path: path.iter().map(|s| s.to_string()).collect(),
    })?;

    let thunk = ctx.get_thunk(*thunk_id);
    thunk.try_get_materialized().ok_or_else(|| AstError {
        message: format!("field '{}' is not materialized", key),
        field_path: path.iter().map(|s| s.to_string()).collect(),
    })
}

fn get_string_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<String, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::String(s) => Ok(s),
        _ => Err(AstError {
            message: format!("field '{}' must be String", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_int_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<i64, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Int(n) => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be Int", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_float_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<f64, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Float(f) => Ok(f),
        _ => Err(AstError {
            message: format!("field '{}' must be Float", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_bool_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Bool(b) => Ok(b),
        _ => Err(AstError {
            message: format!("field '{}' must be Bool", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_dict_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Dict(_) => Ok(val),
        _ => Err(AstError {
            message: format!("field '{}' must be Dict", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_optional_dict_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Option<Value>, AstError> {
    match dict.get(&Key::String(key.into())) {
        Some(thunk_id) => {
            let thunk = ctx.get_thunk(*thunk_id);
            Ok(thunk.try_get_materialized())
        }
        None => Ok(None),
    }
}

fn is_empty_dict(val: &Value) -> bool {
    matches!(val, Value::Dict(d) if d.is_empty())
}

fn extract_span(dict: &IndexMap<Key, ThunkId>, ctx: &Rc<crate::eval::EvalContext>) -> Option<Span> {
    let span_thunk_id = dict.get(&Key::String("span".into()))?;
    let span_thunk = ctx.get_thunk(*span_thunk_id);
    let span_val = span_thunk.try_get_materialized()?;

    match span_val {
        Value::Dict(span_dict) => {
            let start_id = span_dict.get(&Key::String("start".into()))?;
            let start_thunk = ctx.get_thunk(*start_id);
            let start_val = start_thunk.try_get_materialized()?;

            let end_id = span_dict.get(&Key::String("end".into()))?;
            let end_thunk = ctx.get_thunk(*end_id);
            let end_val = end_thunk.try_get_materialized()?;

            let start = extract_position(&start_val, ctx)?;
            let end = extract_position(&end_val, ctx)?;

            Some(Span::new(start, end))
        }
        _ => None,
    }
}

fn extract_position(val: &Value, ctx: &Rc<crate::eval::EvalContext>) -> Option<Position> {
    match val {
        Value::Dict(dict) => {
            let line_id = dict.get(&Key::String("line".into()))?;
            let line_thunk = ctx.get_thunk(*line_id);
            let line = match line_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let col_id = dict.get(&Key::String("col".into()))?;
            let col_thunk = ctx.get_thunk(*col_id);
            let column = match col_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let offset_id = dict.get(&Key::String("offset".into()))?;
            let offset_thunk = ctx.get_thunk(*offset_id);
            let offset = match offset_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            Some(Position {
                line,
                column,
                offset,
            })
        }
        _ => None,
    }
}

fn extract_list(
    val: &Value,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Vec<Value>, AstError> {
    match val {
        Value::Dict(d) => {
            // A list is represented as a dict with integer keys 0, 1, 2, ...
            let mut result = Vec::new();
            for i in 0.. {
                match d.get(&Key::Int(i)) {
                    Some(thunk_id) => {
                        let thunk = ctx.get_thunk(*thunk_id);
                        let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                            message: format!("list element {} is not materialized", i),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })?;
                        result.push(val);
                    }
                    None => break,
                }
            }
            Ok(result)
        }
        _ => Err(AstError {
            message: "expected list (dict with integer keys)".into(),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn dict_to_entry(
    val: &Value,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Spanned<Entry>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "entry must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let key_val = get_dict_field(dict, "key", path, ctx)?;
    let key = match &key_val {
        Value::Dict(d) if d.is_empty() => None,
        _ => Some(dict_to_ast(&key_val, ctx)?),
    };

    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = Rc::new(dict_to_ast(&value_val, ctx)?);

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(Entry { key, value }, span))
}

fn dict_to_named_arg(
    val: &Value,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Spanned<NamedArg>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "named-arg must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let name = get_string_field(dict, "name", path, ctx)?;
    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = Rc::new(dict_to_ast(&value_val, ctx)?);

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(NamedArg { name, value }, span))
}

fn dict_to_param(
    val: &Value,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Spanned<Param>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "param must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let name = get_string_field(dict, "name", path, ctx)?;

    let annotation = match get_optional_dict_field(dict, "annotation", ctx)? {
        Some(ann_val) if !is_empty_dict(&ann_val) => {
            let mut new_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            new_path.push("annotation".to_string());
            let path_refs: Vec<&str> = new_path.iter().map(|s| s.as_str()).collect();
            Some(dict_to_annotation(&ann_val, &path_refs, ctx)?)
        }
        _ => None,
    };

    let variadic = get_bool_field(dict, "variadic", path, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(
        Param {
            name,
            annotation,
            variadic,
        },
        span,
    ))
}

fn dict_to_annotation(
    val: &Value,
    path: &[&str],
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<Spanned<Annotation>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "annotation must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let kind = get_string_field(dict, "kind", path, ctx)?;

    let ann = match kind.as_str() {
        "simple" => {
            let value = get_string_field(dict, "value", path, ctx)?;
            Annotation::Simple(value)
        }
        "dict" => {
            let entries_val = get_dict_field(dict, "entries", path, ctx)?;
            let mut entries_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            entries_path.push("entries".to_string());
            let path_refs: Vec<&str> = entries_path.iter().map(|s| s.as_str()).collect();
            let entries_list = extract_list(&entries_val, &path_refs, ctx)?;
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let mut entry_path = entries_path.clone();
                let i_str = i.to_string();
                entry_path.push(i_str.clone());
                let entry_path_refs: Vec<&str> = entry_path.iter().map(|s| s.as_str()).collect();
                let entry = dict_to_entry(&entry_val, &entry_path_refs, ctx)?;
                entries.push(entry);
            }
            Annotation::PropertyDict(entries)
        }
        _ => {
            let mut kind_path = path.to_vec();
            kind_path.push("kind");
            return Err(AstError {
                message: format!("unknown annotation kind: {}", kind),
                field_path: kind_path.iter().map(|s| s.to_string()).collect(),
            });
        }
    };

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(ann, span))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        use crate::value::Environment;
        use std::cell::RefCell;

        let env = Rc::new(RefCell::new(Environment::new()));
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new(base_dir, env, false)
    }

    #[test]
    fn test_ast_to_dict_int() {
        let expr = sp(Expr::Int(42));
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = ast_to_dict_expr(&expr, &opts, &ctx).unwrap();

        match thunk.try_get_materialized() {
            Some(Value::Dict(map)) => {
                // Check type field
                let type_id = map.get(&Key::String("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(
                    type_thunk.try_get_materialized(),
                    Some(Value::String("literal".into()))
                );

                // Check kind field
                let kind_id = map.get(&Key::String("kind".into())).unwrap();
                let kind_thunk = ctx.get_thunk(*kind_id);
                assert_eq!(
                    kind_thunk.try_get_materialized(),
                    Some(Value::String("int".into()))
                );

                // Check value field
                let value_id = map.get(&Key::String("value".into())).unwrap();
                let value_thunk = ctx.get_thunk(*value_id);
                assert_eq!(value_thunk.try_get_materialized(), Some(Value::Int(42)));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_ast_to_dict_var() {
        let expr = sp(Expr::var_ref("x".into()));
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = ast_to_dict_expr(&expr, &opts, &ctx).unwrap();

        match thunk.try_get_materialized() {
            Some(Value::Dict(map)) => {
                let type_id = map.get(&Key::String("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(
                    type_thunk.try_get_materialized(),
                    Some(Value::String("var".into()))
                );

                let name_id = map.get(&Key::String("name".into())).unwrap();
                let name_thunk = ctx.get_thunk(*name_id);
                assert_eq!(
                    name_thunk.try_get_materialized(),
                    Some(Value::String("x".into()))
                );
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_ast_to_dict_file_schema_version() {
        let file = File {
            documents: vec![sp(Document {
                expressions: vec![Rc::new(sp(Expr::Int(1)))],
                name: None,
                output_type: None,
                expects: None,
            })],
        };
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = ast_to_dict(&file, &opts, &ctx).unwrap();

        match thunk.try_get_materialized() {
            Some(Value::Dict(map)) => {
                let type_id = map.get(&Key::String("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(
                    type_thunk.try_get_materialized(),
                    Some(Value::String("file".into()))
                );

                let version_id = map.get(&Key::String("schema-version".into())).unwrap();
                let version_thunk = ctx.get_thunk(*version_id);
                assert_eq!(version_thunk.try_get_materialized(), Some(Value::Int(1)));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_bare_flag_on_bare_word_strings() {
        use crate::parser::parse2;

        // Parse "[foo: 1]" — the key "foo" should have bare: true
        let input = "[foo: 1]";
        let parse_output = parse2(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = ast_to_dict(&parse_output.file.node, &opts, &ctx).unwrap();

        // Navigate to the first document's first expression (the dict)
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        match expr_thunk.try_get_materialized() {
                                            Some(Value::Dict(dict_node)) => {
                                                // Get the entries list
                                                let entries_id = dict_node
                                                    .get(&Key::String("entries".into()))
                                                    .unwrap();
                                                let entries_thunk = ctx.get_thunk(*entries_id);
                                                match entries_thunk.try_get_materialized() {
                                                    Some(Value::Dict(entries_list)) => {
                                                        let entry_id =
                                                            entries_list.get(&Key::Int(0)).unwrap();
                                                        let entry_thunk = ctx.get_thunk(*entry_id);
                                                        match entry_thunk.try_get_materialized() {
                                                            Some(Value::Dict(entry_dict)) => {
                                                                // Get the key expression
                                                                let key_id = entry_dict
                                                                    .get(&Key::String("key".into()))
                                                                    .unwrap();
                                                                let key_thunk =
                                                                    ctx.get_thunk(*key_id);
                                                                match key_thunk
                                                                    .try_get_materialized()
                                                                {
                                                                    Some(Value::Dict(key_dict)) => {
                                                                        // Check bare: true
                                                                        let bare_id = key_dict
                                                                            .get(&Key::String(
                                                                                "bare".into(),
                                                                            ))
                                                                            .expect(
                                                                                "bare field missing",
                                                                            );
                                                                        let bare_thunk =
                                                                            ctx.get_thunk(*bare_id);
                                                                        assert_eq!(
                                                                            bare_thunk
                                                                                .try_get_materialized(),
                                                                            Some(Value::Bool(true)),
                                                                            "bare should be true for bare word 'foo'"
                                                                        );
                                                                    }
                                                                    _ => panic!(
                                                                        "expected Dict for key"
                                                                    ),
                                                                }
                                                            }
                                                            _ => panic!("expected Dict for entry"),
                                                        }
                                                    }
                                                    _ => panic!("expected Dict for entries list"),
                                                }
                                            }
                                            _ => panic!("expected Dict for dict node"),
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_bare_flag_on_quoted_strings() {
        use crate::parser::parse2;

        // Parse "[\"foo\": 1]" — the key "foo" should have bare: false
        let input = "[\"foo\": 1]";
        let parse_output = parse2(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = ast_to_dict(&parse_output.file.node, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        match expr_thunk.try_get_materialized() {
                                            Some(Value::Dict(dict_node)) => {
                                                let entries_id = dict_node
                                                    .get(&Key::String("entries".into()))
                                                    .unwrap();
                                                let entries_thunk = ctx.get_thunk(*entries_id);
                                                match entries_thunk.try_get_materialized() {
                                                    Some(Value::Dict(entries_list)) => {
                                                        let entry_id =
                                                            entries_list.get(&Key::Int(0)).unwrap();
                                                        let entry_thunk = ctx.get_thunk(*entry_id);
                                                        match entry_thunk.try_get_materialized() {
                                                            Some(Value::Dict(entry_dict)) => {
                                                                let key_id = entry_dict
                                                                    .get(&Key::String("key".into()))
                                                                    .unwrap();
                                                                let key_thunk =
                                                                    ctx.get_thunk(*key_id);
                                                                match key_thunk
                                                                    .try_get_materialized()
                                                                {
                                                                    Some(Value::Dict(key_dict)) => {
                                                                        let bare_id = key_dict
                                                                            .get(&Key::String(
                                                                                "bare".into(),
                                                                            ))
                                                                            .expect(
                                                                                "bare field missing",
                                                                            );
                                                                        let bare_thunk =
                                                                            ctx.get_thunk(*bare_id);
                                                                        assert_eq!(
                                                                            bare_thunk
                                                                                .try_get_materialized(),
                                                                            Some(Value::Bool(false)),
                                                                            "bare should be false for quoted string \"foo\""
                                                                        );
                                                                    }
                                                                    _ => panic!(
                                                                        "expected Dict for key"
                                                                    ),
                                                                }
                                                            }
                                                            _ => panic!("expected Dict for entry"),
                                                        }
                                                    }
                                                    _ => panic!("expected Dict for entries list"),
                                                }
                                            }
                                            _ => panic!("expected Dict for dict node"),
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_comment_embedding() {
        use crate::parser::parse2;

        // Parse "[# comment\nx: 1]" — the entry should have leading-comments: [" comment"]
        let input = "[# comment\nx: 1]";
        let parse_output = parse2(input).unwrap();
        let comment_maps = CommentMaps {
            leading_comments: &parse_output.leading_comments,
            trailing_comments: &parse_output.trailing_comments,
            blank_before: &parse_output.blank_before,
        };
        let opts = AstToDictOpts {
            source: Some(input),
            comments: Some(comment_maps),
        };
        let ctx = test_ctx();

        let thunk = ast_to_dict(&parse_output.file.node, &opts, &ctx).unwrap();

        // Navigate to the entry and check for leading-comments
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        match expr_thunk.try_get_materialized() {
                                            Some(Value::Dict(dict_node)) => {
                                                let entries_id = dict_node
                                                    .get(&Key::String("entries".into()))
                                                    .unwrap();
                                                let entries_thunk = ctx.get_thunk(*entries_id);
                                                match entries_thunk.try_get_materialized() {
                                                    Some(Value::Dict(entries_list)) => {
                                                        let entry_id =
                                                            entries_list.get(&Key::Int(0)).unwrap();
                                                        let entry_thunk = ctx.get_thunk(*entry_id);
                                                        match entry_thunk.try_get_materialized() {
                                                            Some(Value::Dict(entry_dict)) => {
                                                                // Check for leading-comments field
                                                                let comments_id = entry_dict
                                                                    .get(&Key::String(
                                                                        "leading-comments".into(),
                                                                    ))
                                                                    .expect("leading-comments field missing");
                                                                let comments_thunk =
                                                                    ctx.get_thunk(*comments_id);
                                                                match comments_thunk
                                                                    .try_get_materialized()
                                                                {
                                                                    Some(Value::Dict(
                                                                        comments_list,
                                                                    )) => {
                                                                        let comment_id =
                                                                            comments_list
                                                                                .get(&Key::Int(0))
                                                                                .expect("comment 0 missing");
                                                                        let comment_thunk =
                                                                            ctx.get_thunk(*comment_id);
                                                                        assert_eq!(
                                                                            comment_thunk
                                                                                .try_get_materialized(),
                                                                            Some(Value::String(" comment".into())),
                                                                            "leading comment should be ' comment'"
                                                                        );
                                                                    }
                                                                    _ => panic!(
                                                                        "expected Dict for comments list"
                                                                    ),
                                                                }
                                                            }
                                                            _ => panic!("expected Dict for entry"),
                                                        }
                                                    }
                                                    _ => panic!("expected Dict for entries list"),
                                                }
                                            }
                                            _ => panic!("expected Dict for dict node"),
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_blank_before_flag() {
        use crate::parser::parse2;
        use std::collections::BTreeMap;

        // Manually inject blank-before data to test the ast_dict lookup.
        // The parser's main loop does not track blank lines between dict entries
        // (skip_whitespace_tokens handles that in specific call sites), so we
        // construct the blank_before map by hand.
        let input = "[a: 1\nb: 2]";
        let parse_output = parse2(input).unwrap();

        // Find the offset of 'b' (the second entry's key).
        // In "[a: 1\nb: 2]": [ at 0, a at 1, : at 2, ' ' at 3, 1 at 4, \n at 5, b at 6
        let mut blank_before_map = BTreeMap::new();
        blank_before_map.insert(6usize, true); // mark 'b' as having a blank line before it
        let comment_maps = CommentMaps {
            leading_comments: &parse_output.leading_comments,
            trailing_comments: &parse_output.trailing_comments,
            blank_before: &blank_before_map,
        };
        let opts = AstToDictOpts {
            source: Some(input),
            comments: Some(comment_maps),
        };
        let ctx = test_ctx();

        let thunk = ast_to_dict(&parse_output.file.node, &opts, &ctx).unwrap();

        // Navigate to the second entry and check blank-before: true
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        match expr_thunk.try_get_materialized() {
                                            Some(Value::Dict(dict_node)) => {
                                                let entries_id = dict_node
                                                    .get(&Key::String("entries".into()))
                                                    .unwrap();
                                                let entries_thunk = ctx.get_thunk(*entries_id);
                                                match entries_thunk.try_get_materialized() {
                                                    Some(Value::Dict(entries_list)) => {
                                                        let entry_id =
                                                            entries_list.get(&Key::Int(1)).unwrap();
                                                        let entry_thunk = ctx.get_thunk(*entry_id);
                                                        match entry_thunk.try_get_materialized() {
                                                            Some(Value::Dict(entry_dict)) => {
                                                                // Check blank-before: true
                                                                let blank_id = entry_dict
                                                                    .get(&Key::String(
                                                                        "blank-before".into(),
                                                                    ))
                                                                    .expect("blank-before field missing");
                                                                let blank_thunk =
                                                                    ctx.get_thunk(*blank_id);
                                                                assert_eq!(
                                                                    blank_thunk
                                                                        .try_get_materialized(),
                                                                    Some(Value::Bool(true)),
                                                                    "blank-before should be true for second entry"
                                                                );
                                                            }
                                                            _ => panic!("expected Dict for entry"),
                                                        }
                                                    }
                                                    _ => panic!("expected Dict for entries list"),
                                                }
                                            }
                                            _ => panic!("expected Dict for dict node"),
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_both_none_mode_unchanged() {
        use crate::parser::parse2;

        // Parse "[foo: 1]" with both source and comments None
        let input = "[foo: 1]";
        let parse_output = parse2(input).unwrap();
        let opts = AstToDictOpts {
            source: None,
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = ast_to_dict(&parse_output.file.node, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false (default when source is None)
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        match expr_thunk.try_get_materialized() {
                                            Some(Value::Dict(dict_node)) => {
                                                let entries_id = dict_node
                                                    .get(&Key::String("entries".into()))
                                                    .unwrap();
                                                let entries_thunk = ctx.get_thunk(*entries_id);
                                                match entries_thunk.try_get_materialized() {
                                                    Some(Value::Dict(entries_list)) => {
                                                        let entry_id =
                                                            entries_list.get(&Key::Int(0)).unwrap();
                                                        let entry_thunk = ctx.get_thunk(*entry_id);
                                                        match entry_thunk.try_get_materialized() {
                                                            Some(Value::Dict(entry_dict)) => {
                                                                let key_id = entry_dict
                                                                    .get(&Key::String("key".into()))
                                                                    .unwrap();
                                                                let key_thunk =
                                                                    ctx.get_thunk(*key_id);
                                                                match key_thunk
                                                                    .try_get_materialized()
                                                                {
                                                                    Some(Value::Dict(key_dict)) => {
                                                                        let bare_id = key_dict
                                                                            .get(&Key::String(
                                                                                "bare".into(),
                                                                            ))
                                                                            .expect(
                                                                                "bare field missing",
                                                                            );
                                                                        let bare_thunk =
                                                                            ctx.get_thunk(*bare_id);
                                                                        assert_eq!(
                                                                            bare_thunk
                                                                                .try_get_materialized(),
                                                                            Some(Value::Bool(false)),
                                                                            "bare should be false when source is None"
                                                                        );
                                                                    }
                                                                    _ => panic!(
                                                                        "expected Dict for key"
                                                                    ),
                                                                }

                                                                // Check that blank-before is still present (always included)
                                                                let blank_id = entry_dict
                                                                    .get(&Key::String(
                                                                        "blank-before".into(),
                                                                    ))
                                                                    .expect("blank-before field missing");
                                                                let blank_thunk =
                                                                    ctx.get_thunk(*blank_id);
                                                                assert_eq!(
                                                                    blank_thunk
                                                                        .try_get_materialized(),
                                                                    Some(Value::Bool(false)),
                                                                    "blank-before should be false when comments is None"
                                                                );

                                                                // Check that leading-comments is absent
                                                                assert!(
                                                                    entry_dict
                                                                        .get(&Key::String(
                                                                            "leading-comments".into()
                                                                        ))
                                                                        .is_none(),
                                                                    "leading-comments should be absent when comments is None"
                                                                );
                                                            }
                                                            _ => panic!("expected Dict for entry"),
                                                        }
                                                    }
                                                    _ => panic!("expected Dict for entries list"),
                                                }
                                            }
                                            _ => panic!("expected Dict for dict node"),
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }
}
