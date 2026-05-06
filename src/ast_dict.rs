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
    /// Comment map from ParseOutput — enables leading-comments, trailing-comment,
    /// and blank-before fields on Entry and Document nodes.
    /// None → no comment fields emitted (compact formatter, quasiquoting).
    pub comments: Option<&'a std::collections::HashMap<usize, Vec<String>>>,
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
    _span: Span,
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

    // blank-before: always false for now (comment support in Phase 2)
    dict.insert(
        Key::String("blank-before".into()),
        ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Bool(false), span))),
    );

    // leading-comments and trailing-comment omitted when comments: None (Phase 1)
    // Phase 2 will populate these from opts.comments

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
    use crate::ast::Position;
    use crate::test_util::sp;

    fn test_span() -> Span {
        Span::new(
            Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            Position {
                offset: 10,
                line: 1,
                column: 11,
            },
        )
    }

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
}
