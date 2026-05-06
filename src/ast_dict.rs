//! AST-to-dict serialization for quasiquoting, macros, and formatter.
//!
//! Converts AST nodes to tinct `Value::Dict` matching the canonical schema
//! defined in `doc/whatif/ast-schema.md`.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{Annotation, Document, DotKey, Entry, Expr, File, NamedArg, Param, Span, Spanned};
use crate::error::EvalResult;
use crate::value::{Key, Thunk, Value};

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
