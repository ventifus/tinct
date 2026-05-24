//! AST-to-dict serialization for quasiquoting, macros, and formatter.
//!
//! Converts AST nodes to tinct `Value::Variant` (Expr nodes) or `Value::Dict`
//! (structural nodes) matching the canonical schema in `doc/feature/ast-schema.md`.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{
    Annotation, Document, DotKey, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned,
    Stage, SurfaceDeclaration, SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceItem,
    SurfaceNamedArg, SurfaceNode, SurfaceParam, SurfaceProgram,
};
use crate::error::EvalResult;
use crate::value::{string_val, Key, Thunk, Value};

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

/// Converts a single expression to a thunk. Used by quasiquoting.
///
/// # Visibility
/// Not part of the public API — callers should use `surface_node_to_dict` instead.
/// Retained for the `annotation_to_thunk_id` fallback path and `#[cfg(test)]` usage.
#[doc(hidden)]
fn ast_to_dict_expr(
    expr: &Spanned<Expr>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    expr_to_thunk(&expr.node, expr.span, opts, ctx)
}

// ============================================================================
// Surface AST Functions (Phases 2-5 — native SurfaceExpression/SurfaceDeclaration path)
// ============================================================================
//
// Phase 2: `surface_node_to_thunk_id` walks `SurfaceExpression` natively for all variants.
// Phase 3: `surface_decl_to_thunk_id` walks `SurfaceDeclaration` natively.
//          `surface_document_to_thunk_id` iterates `SurfaceDocument::items` natively.
// Phase 4: `surface_program_to_dict` rewritten to use native SurfaceDocument iteration.
// Phase 5: `dict_to_surface_node` rewrites the reverse (dict→Surface) direction natively
//          for all variants. Unknown tags return a hard AstError; there is no fallback.
//          `dict_to_surface_program` bridges through `dict_to_file` (old File bridge).
// Phase 6: `ast_to_dict` and `document_to_dict` (old File/Document-based emitters) deleted.
//          `ast_to_dict_expr` retained — still used by `annotation_to_thunk_id` for
//          compound annotation values (non-Str/Int `Expr` nodes in PropertyDict).

/// Convert a SurfaceNode to a dict representation.
///
/// Phase 2 native path: walks `SurfaceExpression` directly without going through
/// `ast_convert`. All `SurfaceExpression` variants are handled natively.
pub fn surface_node_to_dict(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    surface_node_to_dict_inner(node, opts, ctx)
}

/// Walk a SurfaceNode natively, producing the same dict schema as `expr_to_thunk_id`.
///
/// Handles all `SurfaceExpression` variants (Group A from the migration notes) directly,
/// without going through `ast_convert`. Group B variants (`SurfaceDeclaration`) are not
/// `SurfaceExpression` variants and are handled separately via `surface_decl_to_thunk_id`
/// (Phase 3, Step 3 of the migration plan).
fn surface_node_to_dict_inner(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let id = surface_node_to_thunk_id(node, opts, ctx)?;
    Ok(ctx.thunk_arena.lock().unwrap().get(id).clone())
}

/// Convert a SurfaceNode to a ThunkId containing its dict representation.
///
/// This is the surface-native equivalent of `expr_to_thunk_id`. Handles all
/// `SurfaceExpression` variants. Schema (Variant tags, key names) is identical to the
/// old Expr-based emitter — existing tinct metaprogramming code sees no change.
fn surface_node_to_thunk_id(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let expr = &node.expr;
    let span = node.span;

    let capacity = match expr {
        SurfaceExpression::Int(_) | SurfaceExpression::Float(_) | SurfaceExpression::Bool(_) => 2,
        SurfaceExpression::Str(_) => 3,
        SurfaceExpression::VarRef { .. } => 1,
        SurfaceExpression::DotAccess { .. } => 2,
        SurfaceExpression::Pipe { .. } => 2,
        SurfaceExpression::Sequential(_) => 1,
        SurfaceExpression::Dict(_) => 1,
        SurfaceExpression::Call { .. } => 4,
        SurfaceExpression::Fn { .. } => 4,
        SurfaceExpression::TypeAssert { .. } => 2,
        SurfaceExpression::Annotated { .. } => 2,
        SurfaceExpression::Rest(_) => 1,
        SurfaceExpression::Quote(_)
        | SurfaceExpression::Unquote(_)
        | SurfaceExpression::UnquoteSplice(_) => 1,
        SurfaceExpression::Match { .. } => 2,
        SurfaceExpression::PatternDecl { .. } | SurfaceExpression::LetDecl { .. } => 1,
        SurfaceExpression::CaseArm { .. } => 2,
        SurfaceExpression::Placeholder => 0,
        SurfaceExpression::TypeApp { .. } => 2,
        SurfaceExpression::Error(_) => 1,
    };

    let mut dict = IndexMap::with_capacity(capacity);
    let variant_tag: &str;

    match expr {
        SurfaceExpression::Int(n) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("int"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(*n), span))),
            );
        }

        SurfaceExpression::Float(f) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("float"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Float(*f), span))),
            );
        }

        SurfaceExpression::Bool(b) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("bool"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(*b), span))),
            );
        }

        SurfaceExpression::Str(s) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("str"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span))),
            );
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
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(bare), span))),
            );
        }

        SurfaceExpression::VarRef { name, .. } => {
            variant_tag = "VarRef";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
        }

        SurfaceExpression::DotAccess { expr: target, field } => {
            variant_tag = "DotAccess";
            dict.insert(
                Key::String("target".into()),
                surface_node_to_thunk_id(target, opts, ctx)?,
            );
            match field {
                DotKey::Ident(s) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span))),
                    );
                }
                DotKey::Int(n) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(*n), span))),
                    );
                }
            }
        }

        SurfaceExpression::Pipe { lhs, rhs } => {
            variant_tag = "Pipe";
            dict.insert(
                Key::String("lhs".into()),
                surface_node_to_thunk_id(lhs, opts, ctx)?,
            );
            dict.insert(
                Key::String("rhs".into()),
                surface_node_to_thunk_id(rhs, opts, ctx)?,
            );
        }

        SurfaceExpression::Sequential(exprs) => {
            variant_tag = "Sequential";
            let expr_ids: Vec<_> = exprs
                .iter()
                .map(|e| surface_node_to_thunk_id(e, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("exprs".into()),
                list_to_thunk_id(expr_ids.into_iter(), span, ctx)?,
            );
        }

        SurfaceExpression::Dict(entries) => {
            variant_tag = "Dict";
            let entry_ids: Vec<_> = entries
                .iter()
                .map(|e| surface_entry_to_thunk_id(&e.node, e.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids.into_iter(), span, ctx)?,
            );
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            variant_tag = "Call";
            dict.insert(
                Key::String("fn".into()),
                surface_node_to_thunk_id(func, opts, ctx)?,
            );
            let arg_ids: Vec<_> = args
                .iter()
                .map(|a| surface_node_to_thunk_id(a, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("args".into()),
                list_to_thunk_id(arg_ids.into_iter(), span, ctx)?,
            );
            let named_arg_ids: Vec<_> = named_args
                .iter()
                .map(|na| surface_named_arg_to_thunk_id(&na.node, na.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("named-args".into()),
                list_to_thunk_id(named_arg_ids.into_iter(), span, ctx)?,
            );
            dict.insert(
                Key::String("implied".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*implied),
                    span,
                ))),
            );
        }

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            variant_tag = "Fn";
            let param_ids: Vec<_> = params
                .iter()
                .map(|p| surface_param_to_thunk_id(&p.node, span, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("params".into()),
                list_to_thunk_id(param_ids.into_iter(), span, ctx)?,
            );
            dict.insert(
                Key::String("return-ann".into()),
                match return_ann {
                    Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span,
                    ))),
                },
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
            dict.insert(
                Key::String("desugared".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*desugared),
                    span,
                ))),
            );
        }

        SurfaceExpression::TypeAssert { annotation, expr: inner } => {
            variant_tag = "TypeAssert";
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span, ctx)?,
            );
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Annotated { name, annotation } => {
            variant_tag = "Annotated";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span, ctx)?,
            );
        }

        SurfaceExpression::Rest(name_opt) => {
            variant_tag = "Rest";
            dict.insert(
                Key::String("name".into()),
                match name_opt {
                    Some(s) => {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span)))
                    }
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span,
                    ))),
                },
            );
        }

        SurfaceExpression::Quote(inner) => {
            variant_tag = "Quote";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Unquote(inner) => {
            variant_tag = "Unquote";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            variant_tag = "UnquoteSplice";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            variant_tag = "Match";
            dict.insert(
                Key::String("scrutinee".into()),
                surface_node_to_thunk_id(scrutinee, opts, ctx)?,
            );
            let arms_thunks: Vec<ThunkId> = arms
                .iter()
                .map(|arm| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        pattern_to_thunk_id(&arm.pattern.node, arm.pattern.span, ctx)?,
                    );
                    if let Some(guard) = &arm.guard {
                        arm_dict.insert(
                            Key::String("guard".into()),
                            surface_node_to_thunk_id(guard, opts, ctx)?,
                        );
                    }
                    arm_dict.insert(
                        Key::String("body".into()),
                        surface_node_to_thunk_id(&arm.body, opts, ctx)?,
                    );
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(arm_dict),
                        arm.pattern.span,
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;
            let arms_dict: IndexMap<Key, ThunkId> = arms_thunks
                .into_iter()
                .enumerate()
                .map(|(i, id)| (Key::Int(i as i64), id))
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span,
                ))),
            );
        }

        SurfaceExpression::PatternDecl { bindings } => {
            variant_tag = "PatternDecl";
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    Ok((
                        Key::Int(i as i64),
                        surface_node_to_thunk_id(b, opts, ctx)?,
                    ))
                })
                .collect::<EvalResult<IndexMap<_, _>>>()?;
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span,
                ))),
            );
        }

        SurfaceExpression::LetDecl { bindings } => {
            variant_tag = "LetDecl";
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    Ok((
                        Key::Int(i as i64),
                        surface_node_to_thunk_id(b, opts, ctx)?,
                    ))
                })
                .collect::<EvalResult<IndexMap<_, _>>>()?;
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span,
                ))),
            );
        }

        SurfaceExpression::CaseArm { pattern, body } => {
            variant_tag = "CaseArm";
            dict.insert(
                Key::String("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceExpression::Placeholder => {
            variant_tag = "Placeholder";
        }

        SurfaceExpression::TypeApp { func, arg } => {
            variant_tag = "TypeApp";
            dict.insert(
                Key::String("func".into()),
                surface_node_to_thunk_id(func, opts, ctx)?,
            );
            dict.insert(
                Key::String("arg".into()),
                surface_node_to_thunk_id(arg, opts, ctx)?,
            );
        }

        SurfaceExpression::Error(error_span) => {
            variant_tag = "AstError";
            dict.insert(
                Key::String("span".into()),
                span_to_thunk_id(*error_span, ctx)?,
            );
            let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(dict),
                *error_span,
            )));
            return Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Variant {
                    tag: variant_tag.to_string(),
                    payload: Some(payload_id),
                },
                *error_span,
            ))));
        }
    }

    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);
    let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span)));
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Variant {
            tag: variant_tag.to_string(),
            payload: Some(payload_id),
        },
        span,
    ))))
}

/// Convert a `SurfaceDeclaration` to a ThunkId containing its dict representation.
///
/// This is the surface-native handler for Group B variants (compile-time-only declaration
/// forms that moved from `SurfaceExpression` to `SurfaceDeclaration`). Schema (Variant tags,
/// key names) is identical to the old Expr-based emitter — existing tinct metaprogramming
/// code sees no change.
///
/// # `ClassDecl.superclasses`
/// The old Expr-based emitter silently drops `superclasses`. This function continues that
/// behavior — superclasses are not yet represented in the dict schema. Tracked in TODO.md
/// under "grammar-doc-polish: ClassDecl.superclasses silently dropped".
fn surface_decl_to_thunk_id(
    decl: &SurfaceDeclaration,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    let variant_tag: &str;

    match decl {
        SurfaceDeclaration::TypeAlias { params, body } => {
            variant_tag = "TypeAlias";
            if !params.is_empty() {
                let params_thunk_ids: Vec<ThunkId> = params
                    .iter()
                    .map(|p| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(p), span)))
                    })
                    .collect();
                dict.insert(
                    Key::String("params".into()),
                    list_to_thunk_id(params_thunk_ids.into_iter(), span, ctx)?,
                );
            }
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceDeclaration::ClassDecl {
            name,
            params,
            superclasses: _, // TODO (grammar-doc-polish): ClassDecl.superclasses silently dropped — design decision needed on schema representation
            methods,
            determines,
            resolver,
            resolver_injective,
        } => {
            variant_tag = "ClassDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            // params: integer-keyed list of param name strings
            let params_dict: IndexMap<Key, ThunkId> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(p), span))),
                    )
                })
                .collect();
            dict.insert(
                Key::String("params".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(params_dict),
                    span,
                ))),
            );
            // methods: string-keyed dict of method expression dicts
            // Keys are SurfaceExpression::Str bare words; values are the full entry value nodes.
            let methods_dict: IndexMap<Key, ThunkId> = methods
                .iter()
                .filter_map(|method| {
                    method.node.key.as_ref().and_then(|key| {
                        if let SurfaceExpression::Str(key_str) = &key.expr {
                            Some((
                                Key::String(Rc::from(key_str.as_str())),
                                surface_node_to_thunk_id(&method.node.value, opts, ctx).ok()?,
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            dict.insert(
                Key::String("methods".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(methods_dict),
                    span,
                ))),
            );
            // determines: optional integer-keyed list of expression dicts
            if !determines.is_empty() {
                let determines_dict: IndexMap<Key, ThunkId> = determines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, fd_node)| {
                        Some((
                            Key::Int(i as i64),
                            surface_node_to_thunk_id(fd_node, opts, ctx).ok()?,
                        ))
                    })
                    .collect();
                dict.insert(
                    Key::String("determines".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(determines_dict),
                        span,
                    ))),
                );
            }
            // resolver: optional expression dict
            if let Some(resolver_node) = resolver {
                dict.insert(
                    Key::String("resolver".into()),
                    surface_node_to_thunk_id(resolver_node, opts, ctx)?,
                );
            }
            // injective: optional bool (only emitted when true)
            if *resolver_injective {
                dict.insert(
                    Key::String("injective".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(true), span))),
                );
            }
        }

        SurfaceDeclaration::InstanceDecl { class_name, arms } => {
            variant_tag = "InstanceDecl";
            dict.insert(
                Key::String("class".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(class_name),
                    span,
                ))),
            );
            // arms: integer-keyed list of {pattern, methods} dicts
            let arms_dict: IndexMap<Key, ThunkId> = arms
                .iter()
                .enumerate()
                .filter_map(|(i, (pattern_node, methods))| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        surface_node_to_thunk_id(pattern_node, opts, ctx).ok()?,
                    );
                    // methods: string-keyed dict matching ClassDecl.methods format
                    let methods_dict: IndexMap<Key, ThunkId> = methods
                        .iter()
                        .filter_map(|method| {
                            method.node.key.as_ref().and_then(|key| {
                                if let SurfaceExpression::Str(key_str) = &key.expr {
                                    Some((
                                        Key::String(Rc::from(key_str.as_str())),
                                        surface_node_to_thunk_id(&method.node.value, opts, ctx)
                                            .ok()?,
                                    ))
                                } else {
                                    None
                                }
                            })
                        })
                        .collect();
                    arm_dict.insert(
                        Key::String("methods".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(methods_dict),
                            span,
                        ))),
                    );
                    Some((
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(arm_dict),
                            span,
                        ))),
                    ))
                })
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span,
                ))),
            );
        }

        SurfaceDeclaration::DefMacro { name, params, body } => {
            variant_tag = "DefMacro";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("params".into()),
                surface_node_to_thunk_id(params, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceDeclaration::MacroDecl { name, params, body } => {
            variant_tag = "MacroDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("params".into()),
                surface_node_to_thunk_id(params, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceDeclaration::SyntaxClass { name, pattern, message } => {
            variant_tag = "SyntaxClass";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            if let Some(msg) = message {
                dict.insert(
                    Key::String("message".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(msg), span))),
                );
            }
        }

        SurfaceDeclaration::Splice(forms) => {
            variant_tag = "Splice";
            let mut form_list = Vec::new();
            for form in forms {
                form_list.push(surface_node_to_thunk_id(form, opts, ctx)?);
            }
            dict.insert(
                Key::String("forms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(
                        form_list
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| (Key::Int(i as i64), v))
                            .collect(),
                    ),
                    span,
                ))),
            );
        }
    }

    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);
    let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span)));
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Variant {
            tag: variant_tag.to_string(),
            payload: Some(payload_id),
        },
        span,
    ))))
}

/// Convert a `SurfaceDocument` to a ThunkId containing its dict representation.
///
/// Phase 3 native path: iterates `doc.items` directly (instead of `doc.expressions`),
/// dispatching on `SurfaceItem::Expr` → `surface_node_to_thunk_id` and
/// `SurfaceItem::Decl` → `surface_decl_to_thunk_id`. The emitted schema is identical
/// to the old `document_to_dict` function — all fields and their types are unchanged.
fn surface_document_to_thunk_id(
    doc: &SurfaceDocument,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("document"),
            span,
        ))),
    );

    // expressions: list of expression/declaration dicts (all SurfaceItems, both Expr and Decl)
    let item_ids: Vec<_> = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node) => surface_node_to_thunk_id(node, opts, ctx),
            SurfaceItem::Decl(decl) => surface_decl_to_thunk_id(&decl.node, decl.span, opts, ctx),
        })
        .collect::<EvalResult<Vec<_>>>()?;

    dict.insert(
        Key::String("expressions".into()),
        list_to_thunk_id(item_ids.into_iter(), span, ctx)?,
    );

    // name: string or []
    dict.insert(
        Key::String("name".into()),
        match &doc.name {
            Some(s) => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span))),
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    // stage: [Runtime] | [Type] — nominal variant based on document stage annotation
    let stage_tag = match &doc.stage {
        Some(Stage::Type) => "Type",
        Some(Stage::Runtime) | None => "Runtime",
    };
    dict.insert(
        Key::String("stage".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: stage_tag.to_string(),
                payload: None,
            },
            span,
        ))),
    );

    // leading-comments: absent when None or empty
    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&span.start.offset) {
            if !comments.is_empty() {
                let comment_ids: Vec<ThunkId> = comments
                    .iter()
                    .map(|c| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(c), span)))
                    })
                    .collect();
                dict.insert(
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span, ctx)?,
                );
            }
        }
    }

    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `entry_to_thunk_id`. Uses `SurfaceEntry` instead of `Entry`.
fn surface_entry_to_thunk_id(
    entry: &SurfaceEntry,
    entry_span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let span = entry.value.span;
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("entry"), span))),
    );

    dict.insert(
        Key::String("key".into()),
        match &entry.key {
            Some(k) => surface_node_to_thunk_id(k, opts, ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    dict.insert(
        Key::String("value".into()),
        surface_node_to_thunk_id(&entry.value, opts, ctx)?,
    );

    let blank_before = opts
        .comments
        .as_ref()
        .and_then(|maps| maps.blank_before.get(&entry_span.start.offset))
        .copied()
        .unwrap_or(false);
    dict.insert(
        Key::String("blank-before".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Bool(blank_before),
            span,
        ))),
    );

    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&entry_span.start.offset) {
            if !comments.is_empty() {
                let comment_ids: Vec<ThunkId> = comments
                    .iter()
                    .map(|c| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(c), span)))
                    })
                    .collect();
                dict.insert(
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span, ctx)?,
                );
            }
        }
        if let Some(comment) = comment_maps.trailing_comments.get(&entry_span.start.offset) {
            dict.insert(
                Key::String("trailing-comment".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(comment), span))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `named_arg_to_thunk_id`. Uses `SurfaceNamedArg`.
fn surface_named_arg_to_thunk_id(
    named_arg: &SurfaceNamedArg,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&named_arg.name),
            span,
        ))),
    );
    dict.insert(
        Key::String("value".into()),
        surface_node_to_thunk_id(&named_arg.value, opts, ctx)?,
    );
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `param_to_thunk_id`. Uses `SurfaceParam`.
fn surface_param_to_thunk_id(
    param: &SurfaceParam,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&param.name),
            span,
        ))),
    );
    dict.insert(
        Key::String("annotation".into()),
        match &param.annotation {
            Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );
    dict.insert(
        Key::String("variadic".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Bool(param.variadic),
            span,
        ))),
    );
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a SurfaceProgram to a dict representation.
///
/// Phase 4 native path: iterates `program.documents` directly, emitting each document
/// via `surface_document_to_thunk_id`. No longer bridges through `ast_convert`.
/// Schema matches the canonical AST schema in `doc/feature/ast-schema.md` — existing
/// tinct metaprogramming code sees no change.
pub fn surface_program_to_dict(
    program: &SurfaceProgram,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = program
        .documents
        .first()
        .map(|d| d.span)
        .unwrap_or_else(Span::origin);
    let mut root = IndexMap::new();

    root.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("file"), span))),
    );

    root.insert(
        Key::String("schema-version".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
    );

    // documents: list of document dicts
    let docs: Vec<_> = program
        .documents
        .iter()
        .map(|doc| surface_document_to_thunk_id(&doc.node, doc.span, opts, ctx))
        .collect::<EvalResult<Vec<_>>>()?;

    root.insert(
        Key::String("documents".into()),
        list_to_thunk_id(docs.into_iter(), span, ctx)?,
    );

    root.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    Ok(Arc::new(Thunk::new_materialized(Value::Dict(root), span)))
}

/// Convert a dict representation back to a SurfaceNode.
///
/// Reads the Variant tag or `type:` field and dispatches to the native `SurfaceExpression`
/// constructor. All variants are handled natively. Unknown tags return a hard `AstError`;
/// there is no Expr-based fallback path.
pub fn dict_to_surface_node(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    dict_to_surface_node_inner(val, ctx)
}

/// Inner implementation of `dict_to_surface_node`.
///
/// Extracts the Variant tag (or legacy `type:` string) and dispatches to the native
/// `SurfaceExpression` constructor. All known variants are handled here. Unknown tags
/// return a hard `AstError` — there is no Expr-based fallback bridge.
///
/// Variants handled here:
/// - `"VarRef"`, `"Literal"` (Int/Float/Bool/Str), `"Dict"`, `"Fn"`,
///   `"Call"`, `"DotAccess"`, `"Pipe"`
fn dict_to_surface_node_inner(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    // Accept both Variant (canonical form from surface_node_to_thunk_id) and
    // Dict (legacy form with type: field).
    let (tag, dict) = match val {
        Value::Variant { tag, payload } => {
            let payload_thunk = payload.as_ref().ok_or_else(|| AstError {
                message: format!("Expr variant {} has no payload", tag),
                field_path: vec![],
            })?;
            let payload_val = ctx
                .get_thunk(*payload_thunk)
                .try_get_materialized()
                .ok_or_else(|| AstError {
                    message: "variant payload is not materialized".into(),
                    field_path: vec![],
                })?;
            match payload_val {
                Value::Dict(d) => (tag.clone(), d),
                _ => {
                    return Err(AstError {
                        message: format!(
                            "Expr variant payload must be Dict, got {}",
                            payload_val.type_name()
                        ),
                        field_path: vec![],
                    })
                }
            }
        }
        Value::Dict(d) => {
            let type_str = get_string_field(d, "type", &[], ctx)?;
            (type_str, d.clone())
        }
        _ => {
            return Err(AstError {
                message: "expected Variant or Dict".into(),
                field_path: vec![],
            })
        }
    };

    let span = extract_span(&dict, ctx).unwrap_or_else(Span::origin);

    let expr = match tag.as_str() {
        // ---- Literals (Int, Float, Bool, Str) ----
        "literal" | "Literal" => {
            let kind = get_string_field(&dict, "kind", &["type"], ctx)?;
            match kind.as_str() {
                "int" => {
                    let value = get_int_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Int(value)
                }
                "float" => {
                    let value = get_float_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Float(value)
                }
                "bool" => {
                    let value = get_bool_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Bool(value)
                }
                "str" => {
                    let value = get_string_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Str(value)
                }
                _ => {
                    return Err(AstError {
                        message: format!("unknown literal kind: {}", kind),
                        field_path: vec!["kind".into()],
                    })
                }
            }
        }

        // ---- VarRef ----
        "var" | "VarRef" => {
            let name = get_string_field(&dict, "name", &["type"], ctx)?;
            SurfaceExpression::VarRef { name, escaped: false }
        }

        // ---- DotAccess ----
        "dot-access" | "DotAccess" => {
            let target_val = get_dict_field(&dict, "target", &["type"], ctx)?;
            let target = dict_to_surface_node_inner(&target_val, ctx)?;
            let field_val = get_field(&dict, "field", &["type"], ctx)?;
            let field = match field_val {
                Value::String { ref source, start, end } => {
                    DotKey::Ident(source[start..end].to_string())
                }
                Value::Int(n) => DotKey::Int(n),
                _ => {
                    return Err(AstError {
                        message: "field must be String or Int".into(),
                        field_path: vec!["field".into()],
                    })
                }
            };
            SurfaceExpression::DotAccess { expr: target, field }
        }

        // ---- Pipe ----
        "pipe" | "Pipe" => {
            let lhs_val = get_dict_field(&dict, "lhs", &["type"], ctx)?;
            let rhs_val = get_dict_field(&dict, "rhs", &["type"], ctx)?;
            SurfaceExpression::Pipe {
                lhs: dict_to_surface_node_inner(&lhs_val, ctx)?,
                rhs: dict_to_surface_node_inner(&rhs_val, ctx)?,
            }
        }

        // ---- Dict ----
        "dict" | "Dict" => {
            let entries_val = get_dict_field(&dict, "entries", &["type"], ctx)?;
            let entries_list = extract_list(&entries_val, &["entries"], ctx)?;
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let entry = dict_to_surface_entry(&entry_val, &["entries", &i_str], ctx)?;
                entries.push(entry);
            }
            SurfaceExpression::Dict(entries)
        }

        // ---- Call ----
        "call" | "Call" => {
            let fn_val = get_dict_field(&dict, "fn", &["type"], ctx)?;
            let func = dict_to_surface_node_inner(&fn_val, ctx)?;

            let args_val = get_dict_field(&dict, "args", &["type"], ctx)?;
            let args_list = extract_list(&args_val, &["args"], ctx)?;
            let mut args = Vec::new();
            for arg_val in args_list {
                args.push(dict_to_surface_node_inner(&arg_val, ctx)?);
            }

            let named_args_val = get_dict_field(&dict, "named-args", &["type"], ctx)?;
            let named_args_list = extract_list(&named_args_val, &["named-args"], ctx)?;
            let mut named_args = Vec::new();
            for (i, na_val) in named_args_list.into_iter().enumerate() {
                let i_str = i.to_string();
                named_args
                    .push(dict_to_surface_named_arg(&na_val, &["named-args", &i_str], ctx)?);
            }

            let implied = get_bool_field(&dict, "implied", &["type"], ctx)?;

            SurfaceExpression::Call { func, args, named_args, implied }
        }

        // ---- Fn ----
        "fn" | "Fn" => {
            let params_val = get_dict_field(&dict, "params", &["type"], ctx)?;
            let params_list = extract_list(&params_val, &["params"], ctx)?;
            let mut params = Vec::new();
            for (i, param_val) in params_list.into_iter().enumerate() {
                let i_str = i.to_string();
                params.push(dict_to_surface_param(&param_val, &["params", &i_str], ctx)?);
            }

            let return_ann = match get_optional_dict_field(&dict, "return-ann", ctx)? {
                Some(ann_val) if !is_empty_dict(&ann_val) => {
                    Some(dict_to_annotation(&ann_val, &["return-ann"], ctx)?)
                }
                _ => None,
            };

            let body_val = get_dict_field(&dict, "body", &["type"], ctx)?;
            let body = dict_to_surface_node_inner(&body_val, ctx)?;

            let desugared = get_bool_field(&dict, "desugared", &["type"], ctx)?;

            SurfaceExpression::Fn { return_ann, params, body, desugared }
        }

        // ---- Unknown variant: hard error (all variants must be handled natively) ----
        _ => {
            return Err(AstError {
                message: format!("dict_to_surface_node: unknown node type: {}", tag),
                field_path: vec![],
            });
        }
    };

    Ok(Arc::new(SurfaceNode { expr, span }))
}

/// Convert a dict to a `Spanned<SurfaceEntry>`.
///
/// Surface-native reverse for `surface_entry_to_thunk_id`.
fn dict_to_surface_entry(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceEntry>, AstError> {
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
    let key: Option<Arc<SurfaceNode>> = match &key_val {
        Value::Dict(d) if d.is_empty() => None,
        _ => Some(dict_to_surface_node_inner(&key_val, ctx)?),
    };

    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = dict_to_surface_node_inner(&value_val, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(SurfaceEntry { key, value }, span))
}

/// Convert a dict to a `Spanned<SurfaceNamedArg>`.
///
/// Surface-native reverse for `surface_named_arg_to_thunk_id`.
fn dict_to_surface_named_arg(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceNamedArg>, AstError> {
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
    let value = dict_to_surface_node_inner(&value_val, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(SurfaceNamedArg { name, value }, span))
}

/// Convert a dict to a `Spanned<SurfaceParam>`.
///
/// Surface-native reverse for `surface_param_to_thunk_id`.
fn dict_to_surface_param(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceParam>, AstError> {
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

    Ok(Spanned::new(SurfaceParam { name, annotation, variadic }, span))
}

/// Convert a dict representation back to a SurfaceProgram.
///
/// Bridges through the old File-based `dict_to_file` path. The reverse (dict→Surface)
/// native rewrite is Step 6 of the migration plan in
/// `doc/whatif/plans/ast-dict-surface-migration-notes.md`.
pub fn dict_to_surface_program(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<SurfaceProgram, AstError> {
    let file = dict_to_file(val, ctx)?;
    Ok(crate::ast_convert::file_to_surface_program(&file))
}

// ============================================================================
// Internal Implementation (Expr-based helpers)
//
// These functions back `ast_to_dict_expr` (still used by `annotation_to_thunk_id`
// for compound PropertyDict values) and `dict_to_ast`/`dict_to_file` (used by
// `dict_to_surface_program` and `#[cfg(test)]` usage).
// ============================================================================

fn expr_to_thunk(
    expr: &Expr,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let id = expr_to_thunk_id(expr, span, opts, ctx)?;
    Ok(ctx.thunk_arena.lock().unwrap().get(id).clone())
}

fn expr_to_thunk_id(
    expr: &Expr,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    // Pre-compute capacity for the payload dict based on the expression variant.
    // Most variants have 1-4 fields. Using with_capacity avoids reallocation during insertion.
    let capacity = match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) => 2, // kind + value
        Expr::Str(_) => 3,                                  // kind + value + bare
        Expr::VarRef { .. } => 1,                           // name
        Expr::DotAccess { .. } => 2,                        // target + field
        Expr::Pipe { .. } => 2,                             // lhs + rhs
        Expr::Sequential(_) => 1,                           // exprs
        Expr::Dict(_) => 1,                                 // entries
        Expr::Call { .. } => 4,                             // fn + args + named-args + implied
        Expr::Fn { .. } => 4,         // params + return-ann + body + desugared
        Expr::TypeAlias { .. } => 2,  // params + body
        Expr::TypeAssert { .. } => 2, // expr + type_expr
        Expr::Annotated { .. } => 2,  // name + annotation
        Expr::Rest(_) => 1,           // name (optional)
        Expr::Quote(_) | Expr::Unquote(_) | Expr::UnquoteSplice(_) => 1, // expr
        Expr::DefMacro { .. } | Expr::MacroDecl { .. } => 3, // name + params + body
        Expr::Splice(_) => 1,         // forms
        Expr::SyntaxClass { .. } => 2, // name + fields
        Expr::Match { .. } => 2,      // scrutinee + arms
        Expr::ClassDecl { .. } => 4,  // name + params + fields + methods
        Expr::InstanceDecl { .. } => 2, // class_name + arms
        Expr::PatternDecl { .. } | Expr::LetDecl { .. } => 1, // bindings
        Expr::CaseArm { .. } => 2,    // pattern + body
        Expr::Placeholder => 0,       // no fields
        Expr::TypeApp { .. } => 2,    // func + arg
        Expr::Error(_) => 1,          // span
    };

    let mut dict = IndexMap::with_capacity(capacity);

    // Track which Variant tag to use for this Expr type
    let variant_tag: &str;

    match expr {
        Expr::Int(n) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("int"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(*n), span))),
            );
        }

        Expr::Float(f) => {
            variant_tag = "Literal";

            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("float"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Float(*f), span))),
            );
        }

        Expr::Bool(b) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("bool"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(*b), span))),
            );
        }

        Expr::Str(s) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("str"), span))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span))),
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
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(bare), span))),
            );
        }

        Expr::VarRef { name, .. } => {
            variant_tag = "VarRef";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
        }

        Expr::DotAccess {
            expr: target,
            field,
        } => {
            variant_tag = "DotAccess";
            dict.insert(
                Key::String("target".into()),
                expr_to_thunk_id(&target.node, target.span, opts, ctx)?,
            );

            // field is either String or Int
            match field {
                DotKey::Ident(s) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span))),
                    );
                }
                DotKey::Int(n) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(*n), span))),
                    );
                }
            }
        }

        Expr::Pipe { lhs, rhs } => {
            variant_tag = "Pipe";
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
            variant_tag = "Sequential";
            let expr_ids: Vec<_> = exprs
                .iter()
                .map(|e| expr_to_thunk_id(&e.node, e.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("exprs".into()),
                list_to_thunk_id(expr_ids.into_iter(), span, ctx)?,
            );
        }

        Expr::Dict(entries) => {
            variant_tag = "Dict";
            let entry_ids: Vec<_> = entries
                .iter()
                .map(|e| entry_to_thunk_id(&e.node, e.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids.into_iter(), span, ctx)?,
            );
        }

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            variant_tag = "Call";
            dict.insert(
                Key::String("fn".into()),
                expr_to_thunk_id(&func.node, func.span, opts, ctx)?,
            );

            // args: list of expression dicts
            let arg_ids: Vec<_> = args
                .iter()
                .map(|a| expr_to_thunk_id(&a.node, a.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("args".into()),
                list_to_thunk_id(arg_ids.into_iter(), span, ctx)?,
            );

            // named-args: list of [name: str value: expr] dicts
            let named_arg_ids: Vec<_> = named_args
                .iter()
                .map(|na| named_arg_to_thunk_id(&na.node, na.span, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("named-args".into()),
                list_to_thunk_id(named_arg_ids.into_iter(), span, ctx)?,
            );
            dict.insert(
                Key::String("implied".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
            variant_tag = "Fn";
            // params: list of param dicts
            let param_ids: Vec<_> = params
                .iter()
                .map(|p| param_to_thunk_id(&p.node, span, ctx))
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("params".into()),
                list_to_thunk_id(param_ids.into_iter(), span, ctx)?,
            );

            // return-ann: annotation or []
            dict.insert(
                Key::String("return-ann".into()),
                match return_ann {
                    Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*desugared),
                    span,
                ))),
            );
        }

        Expr::TypeAlias { params, body } => {
            variant_tag = "TypeAlias";
            if !params.is_empty() {
                // Store params as a dict with integer keys (like other lists)
                let params_thunk_ids: Vec<ThunkId> = params
                    .iter()
                    .map(|p| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(p), span)))
                    })
                    .collect();
                dict.insert(
                    Key::String("params".into()),
                    list_to_thunk_id(params_thunk_ids.into_iter(), span, ctx)?,
                );
            }
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&body.node, body.span, opts, ctx)?,
            );
        }

        Expr::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            variant_tag = "TypeAssert";
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
            variant_tag = "Annotated";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span, ctx)?,
            );
        }

        Expr::Rest(name_opt) => {
            variant_tag = "Rest";
            dict.insert(
                Key::String("name".into()),
                match name_opt {
                    Some(s) => {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(s), span)))
                    }
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span,
                    ))),
                },
            );
        }

        Expr::Quote(inner) => {
            variant_tag = "Quote";
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::Unquote(inner) => {
            variant_tag = "Unquote";
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::UnquoteSplice(inner) => {
            variant_tag = "UnquoteSplice";
            dict.insert(
                Key::String("expr".into()),
                expr_to_thunk_id(&inner.node, inner.span, opts, ctx)?,
            );
        }

        Expr::DefMacro { name, params, body } => {
            variant_tag = "DefMacro";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            // Convert params (now a LetDecl Expr) to dict representation
            dict.insert(
                Key::String("params".into()),
                expr_to_thunk_id(&params.node, params.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                expr_to_thunk_id(&body.node, body.span, opts, ctx)?,
            );
        }

        Expr::MacroDecl { name, params, body } => {
            variant_tag = "MacroDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("params".into()),
                expr_to_thunk_id(&params.node, params.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                expr_to_thunk_id(&body.node, body.span, opts, ctx)?,
            );
        }

        Expr::Splice(forms) => {
            variant_tag = "Splice";
            let mut form_list = Vec::new();
            for form in forms {
                form_list.push(expr_to_thunk_id(&form.node, form.span, opts, ctx)?);
            }
            dict.insert(
                Key::String("forms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(
                        form_list
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| (Key::Int(i as i64), v))
                            .collect(),
                    ),
                    span,
                ))),
            );
        }

        Expr::SyntaxClass {
            name,
            pattern,
            message,
        } => {
            variant_tag = "SyntaxClass";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("pattern".into()),
                expr_to_thunk_id(&pattern.node, pattern.span, opts, ctx)?,
            );
            if let Some(msg) = message {
                dict.insert(
                    Key::String("message".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(msg), span))),
                );
            }
        }

        Expr::Match { scrutinee, arms } => {
            variant_tag = "Match";
            dict.insert(
                Key::String("scrutinee".into()),
                expr_to_thunk_id(&scrutinee.node, scrutinee.span, opts, ctx)?,
            );
            // Serialize arms as a list
            let arms_thunks: Vec<ThunkId> = arms
                .iter()
                .map(|arm| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        pattern_to_thunk_id(&arm.pattern.node, arm.pattern.span, ctx)?,
                    );
                    if let Some(guard) = &arm.guard {
                        arm_dict.insert(
                            Key::String("guard".into()),
                            expr_to_thunk_id(&guard.node, guard.span, opts, ctx)?,
                        );
                    }
                    arm_dict.insert(
                        Key::String("body".into()),
                        expr_to_thunk_id(&arm.body.node, arm.body.span, opts, ctx)?,
                    );
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span,
                ))),
            );
        }

        Expr::ClassDecl {
            name,
            params,
            superclasses: _, // TODO (grammar-doc-polish): ClassDecl.superclasses silently dropped — design decision needed on schema representation
            methods,
            determines,
            resolver,
            resolver_injective,
        } => {
            variant_tag = "ClassDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            // Serialize params as a list
            let params_dict: IndexMap<Key, ThunkId> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(p), span))),
                    )
                })
                .collect();
            dict.insert(
                Key::String("params".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(params_dict),
                    span,
                ))),
            );
            // Serialize methods as a dict
            let methods_dict: IndexMap<Key, ThunkId> = methods
                .iter()
                .filter_map(|method| {
                    method.node.key.as_ref().and_then(|key| {
                        if let Expr::Str(key_str) = &key.node {
                            Some((
                                Key::String(Rc::from(key_str.as_str())),
                                expr_to_thunk_id(
                                    &method.node.value.node,
                                    method.node.value.span,
                                    opts,
                                    ctx,
                                )
                                .ok()?,
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            dict.insert(
                Key::String("methods".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(methods_dict),
                    span,
                ))),
            );
            // Serialize determines as a list
            if !determines.is_empty() {
                let determines_dict: IndexMap<Key, ThunkId> = determines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, fd_expr)| {
                        Some((
                            Key::Int(i as i64),
                            expr_to_thunk_id(&fd_expr.node, fd_expr.span, opts, ctx).ok()?,
                        ))
                    })
                    .collect();
                dict.insert(
                    Key::String("determines".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(determines_dict),
                        span,
                    ))),
                );
            }
            // Serialize resolver
            if let Some(resolver_expr) = resolver {
                dict.insert(
                    Key::String("resolver".into()),
                    expr_to_thunk_id(&resolver_expr.node, resolver_expr.span, opts, ctx)?,
                );
            }
            // Serialize resolver_injective
            if *resolver_injective {
                dict.insert(
                    Key::String("injective".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(true), span))),
                );
            }
        }

        Expr::InstanceDecl { class_name, arms } => {
            variant_tag = "InstanceDecl";
            dict.insert(
                Key::String("class".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(class_name),
                    span,
                ))),
            );
            // Serialize arms as a list
            let arms_dict: IndexMap<Key, ThunkId> = arms
                .iter()
                .enumerate()
                .filter_map(|(i, (pattern_expr, methods))| {
                    // Build arm dict with pattern and methods
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        expr_to_thunk_id(&pattern_expr.node, pattern_expr.span, opts, ctx).ok()?,
                    );
                    let methods_dict: IndexMap<Key, ThunkId> = methods
                        .iter()
                        .filter_map(|method| {
                            method.node.key.as_ref().and_then(|key| {
                                if let Expr::Str(key_str) = &key.node {
                                    Some((
                                        Key::String(Rc::from(key_str.as_str())),
                                        expr_to_thunk_id(
                                            &method.node.value.node,
                                            method.node.value.span,
                                            opts,
                                            ctx,
                                        )
                                        .ok()?,
                                    ))
                                } else {
                                    None
                                }
                            })
                        })
                        .collect();
                    arm_dict.insert(
                        Key::String("methods".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(methods_dict),
                            span,
                        ))),
                    );
                    Some((
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(arm_dict),
                            span,
                        ))),
                    ))
                })
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span,
                ))),
            );
        }

        Expr::PatternDecl { bindings } => {
            variant_tag = "PatternDecl";
            // Serialize bindings as a list
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .filter_map(|(i, binding)| {
                    Some((
                        Key::Int(i as i64),
                        expr_to_thunk_id(&binding.node, binding.span, opts, ctx).ok()?,
                    ))
                })
                .collect();
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span,
                ))),
            );
        }

        Expr::LetDecl { bindings } => {
            variant_tag = "LetDecl";
            // Serialize bindings as a list
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .filter_map(|(i, binding)| {
                    Some((
                        Key::Int(i as i64),
                        expr_to_thunk_id(&binding.node, binding.span, opts, ctx).ok()?,
                    ))
                })
                .collect();
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span,
                ))),
            );
        }

        Expr::CaseArm { pattern, body } => {
            variant_tag = "CaseArm";
            dict.insert(
                Key::String("pattern".into()),
                expr_to_thunk_id(&pattern.node, pattern.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                expr_to_thunk_id(&body.node, body.span, opts, ctx)?,
            );
        }

        Expr::Placeholder => {
            variant_tag = "Placeholder";
        }

        Expr::TypeApp { func, arg } => {
            variant_tag = "TypeApp";
            dict.insert(
                Key::String("func".into()),
                expr_to_thunk_id(&func.node, func.span, opts, ctx)?,
            );
            dict.insert(
                Key::String("arg".into()),
                expr_to_thunk_id(&arg.node, arg.span, opts, ctx)?,
            );
        }

        Expr::Error(error_span) => {
            variant_tag = "AstError";
            // Use the error's own span, not the outer span
            dict.insert(
                Key::String("span".into()),
                span_to_thunk_id(*error_span, ctx)?,
            );
            // Wrap in variant and return early
            let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(dict),
                *error_span,
            )));
            return Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Variant {
                    tag: variant_tag.to_string(),
                    payload: Some(payload_id),
                },
                *error_span,
            ))));
        }
    }

    // Add span to every node (unless it's Error which handles its own span)
    dict.insert(Key::String("span".into()), span_to_thunk_id(span, ctx)?);

    // Wrap the dict in a Variant with the tag determined by the Expr variant
    let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span)));
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Variant {
            tag: variant_tag.to_string(),
            payload: Some(payload_id),
        },
        span,
    ))))
}

/// Convert a pattern to a ThunkId containing a dict representation.
fn pattern_to_thunk_id(
    pattern: &crate::ast::Pattern,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    use crate::ast::{LiteralPattern, Pattern};
    let mut dict = IndexMap::new();

    match pattern {
        Pattern::Wildcard => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("wildcard"),
                    span,
                ))),
            );
        }
        Pattern::Variable(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("variable"),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
        }
        Pattern::TypeTag(tag) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("type_tag"),
                    span,
                ))),
            );
            dict.insert(
                Key::String("tag".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(tag), span))),
            );
        }
        Pattern::Pin(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("pin"), span))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
        }
        Pattern::Literal(lit) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("literal"),
                    span,
                ))),
            );
            let value = match lit {
                LiteralPattern::Int(n) => Value::Int(*n),
                LiteralPattern::Float(f) => Value::Float(*f),
                LiteralPattern::Bool(b) => Value::Bool(*b),
                LiteralPattern::Str(s) => string_val(s),
            };
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(value, span))),
            );
        }
        Pattern::Dict { fields, rest } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("dict"), span))),
            );
            // Convert fields to a dict
            let mut fields_dict = IndexMap::new();
            for (i, (key, pat)) in fields.iter().enumerate() {
                let mut field_dict = IndexMap::new();
                field_dict.insert(
                    Key::String("key".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(key), pat.span))),
                );
                field_dict.insert(
                    Key::String("pattern".into()),
                    pattern_to_thunk_id(&pat.node, pat.span, ctx)?,
                );
                fields_dict.insert(
                    Key::String(Rc::from(i.to_string().as_str())),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(field_dict),
                        pat.span,
                    ))),
                );
            }
            dict.insert(
                Key::String("fields".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(fields_dict),
                    span,
                ))),
            );
            dict.insert(
                Key::String("rest".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Bool(*rest), span))),
            );
        }
        Pattern::Seq { head, tail } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("seq"), span))),
            );
            dict.insert(
                Key::String("head".into()),
                pattern_to_thunk_id(&head.node, head.span, ctx)?,
            );
            dict.insert(
                Key::String("tail".into()),
                pattern_to_thunk_id(&tail.node, tail.span, ctx)?,
            );
        }
        Pattern::Constructor { tag, binding } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("constructor"),
                    span,
                ))),
            );
            dict.insert(
                Key::String("tag".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(tag), span))),
            );
            if let Some(pat) = binding {
                dict.insert(
                    Key::String("binding".into()),
                    pattern_to_thunk_id(&pat.node, pat.span, ctx)?,
                );
            }
        }
        Pattern::Or(patterns) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("or"), span))),
            );
            let pattern_thunks: Vec<_> = patterns
                .iter()
                .map(|pat| pattern_to_thunk_id(&pat.node, pat.span, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            let patterns_dict: IndexMap<Key, ThunkId> = pattern_thunks
                .into_iter()
                .enumerate()
                .map(|(i, thunk_id)| (Key::Int(i as i64), thunk_id))
                .collect();
            dict.insert(
                Key::String("patterns".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(patterns_dict),
                    span,
                ))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn entry_to_thunk_id(
    entry: &Entry,
    entry_span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let span = entry.value.span;
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("entry"), span))),
    );

    // key: expression or []
    dict.insert(
        Key::String("key".into()),
        match &entry.key {
            Some(k) => expr_to_thunk_id(&k.node, k.span, opts, ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
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
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(c), span)))
                    })
                    .collect();
                dict.insert(
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span, ctx)?,
                );
            }
        }

        // trailing-comment: absent when None
        if let Some(comment) = comment_maps.trailing_comments.get(&entry_span.start.offset) {
            dict.insert(
                Key::String("trailing-comment".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(comment), span))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn named_arg_to_thunk_id(
    named_arg: &NamedArg,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&named_arg.name),
            span,
        ))),
    );
    dict.insert(
        Key::String("value".into()),
        expr_to_thunk_id(&named_arg.value.node, named_arg.value.span, opts, ctx)?,
    );
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn param_to_thunk_id(
    param: &Param,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&param.name),
            span,
        ))),
    );

    // annotation: annotation or []
    dict.insert(
        Key::String("annotation".into()),
        match &param.annotation {
            Some(a) => annotation_to_thunk_id(&a.node, span, ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        },
    );

    dict.insert(
        Key::String("variadic".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Bool(param.variadic),
            span,
        ))),
    );

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn annotation_to_thunk_id(
    ann: &Annotation,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("annotation"),
            span,
        ))),
    );

    match ann {
        Annotation::Simple(name) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("simple"),
                    span,
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
        }
        Annotation::Annotated(name, inner) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("annotated"),
                    span,
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val(name), span))),
            );
            dict.insert(
                Key::String("inner".into()),
                annotation_to_thunk_id(inner, span, ctx)?,
            );
        }
        Annotation::PropertyDict(entries) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(string_val("dict"), span))),
            );

            // Convert entries to thunk IDs - these are annotation entries (simpler than regular entries)
            let entry_ids: Vec<_> = entries
                .iter()
                .map(|e| {
                    let mut entry_dict = IndexMap::new();
                    entry_dict.insert(
                        Key::String("type".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val("entry"),
                            e.span,
                        ))),
                    );

                    // For annotation dicts, keys are always string literals (bare words)
                    let key_id = match &e.node.key {
                        Some(k) => match &k.node {
                            Expr::Str(s) => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(s),
                                k.span,
                            ))),
                            _ => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                Value::Dict(IndexMap::new()),
                                k.span,
                            ))),
                        },
                        None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(IndexMap::new()),
                            e.span,
                        ))),
                    };

                    entry_dict.insert(Key::String("key".into()), key_id);

                    // Annotation entry values are strings/ints for simple cases,
                    // or full AST dicts for compound values like [a: Numeric] or Seq@Int.
                    let value_id = match &e.node.value.node {
                        Expr::Str(s) => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(s),
                            e.node.value.span,
                        ))),
                        Expr::Int(n) => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Int(*n),
                            e.node.value.span,
                        ))),
                        _ => ctx.alloc_thunk(ast_to_dict_expr(
                            &e.node.value,
                            &AstToDictOpts::default(),
                            ctx,
                        )?),
                    };

                    entry_dict.insert(Key::String("value".into()), value_id);
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(entry_dict),
                        e.span,
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids.into_iter(), span, ctx)?,
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn span_to_thunk_id(span: Span, ctx: &Arc<crate::eval::EvalContext>) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    // start position
    let mut start_dict = IndexMap::new();
    start_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            span,
        ))),
    );
    start_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            span,
        ))),
    );
    start_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            span,
        ))),
    );

    // end position
    let mut end_dict = IndexMap::new();
    end_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            span,
        ))),
    );
    end_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            span,
        ))),
    );
    end_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            span,
        ))),
    );

    dict.insert(
        Key::String("start".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(start_dict),
            span,
        ))),
    );
    dict.insert(
        Key::String("end".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(end_dict),
            span,
        ))),
    );

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a Vec<ThunkId> to a dict-based list (auto-indexed dict with integer keys).
fn list_to_thunk_id(
    items: impl ExactSizeIterator<Item = ThunkId>,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::with_capacity(items.len());
    for (i, item) in items.enumerate() {
        dict.insert(Key::Int(i as i64), item);
    }
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Converts a tinct dict (materialized Value) back to an `Expr` AST node.
///
/// Validates that the dict conforms to the canonical AST schema. Returns
/// `AstError` if validation fails. Unknown fields are ignored (forward-compatible).
///
/// The `ctx` parameter is needed to dereference ThunkIds embedded in the dict structure.
///
/// **Variant payload constraint**: Variant payloads must be materialized before passing
/// to `dict_to_ast`. Lazy Variant payloads (from `[variant ...]`) will fail here.
/// AST nodes produced by `surface_program_to_dict` use `Thunk::new_materialized` so the
/// round-trip is safe. User code constructing Variant-form AST nodes must call
/// `deep_materialize` on the variant before passing it to this function.
///
/// # Visibility
/// Not part of the public API — callers should use `dict_to_surface_node` instead.
/// Retained as the implementation of `dict_to_file` (backing `dict_to_surface_program`).
#[doc(hidden)]
fn dict_to_ast(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Expr>, AstError> {
    // Accept both Variant (new form) and Dict (legacy form)
    // Strategy: for Variant, recursively call dict_to_ast on the payload
    match val {
        // NEW: Variant form
        Value::Variant { tag, payload } => {
            let payload_thunk = payload.as_ref().ok_or_else(|| AstError {
                message: format!("Expr variant {} has no payload", tag),
                field_path: vec![],
            })?;
            let payload_val = ctx
                .get_thunk(*payload_thunk)
                .try_get_materialized()
                .ok_or_else(|| AstError {
                    message: "variant payload is not materialized".into(),
                    field_path: vec![],
                })?;
            let dict = match payload_val {
                Value::Dict(d) => d,
                _ => {
                    return Err(AstError {
                        message: format!(
                            "Expr variant payload must be Dict, got {}",
                            payload_val.type_name()
                        ),
                        field_path: vec![],
                    })
                }
            };
            // Continue processing with the dict and tag
            dict_to_ast_from_dict(&dict, tag.clone(), ctx)
        }
        // LEGACY: Dict with type: field (backward compat)
        Value::Dict(d) => {
            let type_str = get_string_field(d, "type", &[], ctx)?;
            dict_to_ast_from_dict(d, type_str, ctx)
        }
        _ => Err(AstError {
            message: "expected Variant or Dict".into(),
            field_path: vec![],
        }),
    }
}

// Helper function that does the actual conversion given a dict and type string
fn dict_to_ast_from_dict(
    dict: &IndexMap<Key, ThunkId>,
    type_str: String,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Expr>, AstError> {
    // Extract span (optional — if absent, use synthetic origin span)
    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    let expr = match type_str.as_str() {
        "literal" | "Literal" => {
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

        "var" | "VarRef" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            Expr::VarRef {
                name,
                escaped: false,
                resolved: RefCell::new(None),
            }
        }

        "dot-access" | "DotAccess" => {
            let target_val = get_dict_field(dict, "target", &["type"], ctx)?;
            let target = Box::new(dict_to_ast(&target_val, ctx)?);

            let field_val = get_field(dict, "field", &["type"], ctx)?;
            let field = match field_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => DotKey::Ident(source[start..end].to_string()),
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

        "pipe" | "Pipe" => {
            let lhs_val = get_dict_field(dict, "lhs", &["type"], ctx)?;
            let rhs_val = get_dict_field(dict, "rhs", &["type"], ctx)?;
            Expr::Pipe {
                lhs: Box::new(dict_to_ast(&lhs_val, ctx)?),
                rhs: Box::new(dict_to_ast(&rhs_val, ctx)?),
            }
        }

        "sequential" | "Sequential" => {
            let exprs_val = get_dict_field(dict, "exprs", &["type"], ctx)?;
            let exprs_list = extract_list(&exprs_val, &["exprs"], ctx)?;
            let mut exprs = Vec::new();
            for expr_val in exprs_list.into_iter() {
                let expr = dict_to_ast(&expr_val, ctx)?;
                exprs.push(Rc::new(expr));
            }
            Expr::Sequential(exprs)
        }

        "dict" | "Dict" => {
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

        "call" | "Call" => {
            let fn_val = get_dict_field(dict, "fn", &["type"], ctx)?;
            let func = Box::new(dict_to_ast(&fn_val, ctx)?);

            let args_val = get_dict_field(dict, "args", &["type"], ctx)?;
            let args_list = extract_list(&args_val, &["args"], ctx)?;
            let mut args = Vec::new();
            for arg_val in args_list.into_iter() {
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

        "fn" | "Fn" => {
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

        "type-alias" | "TypeAlias" => {
            let params = match get_optional_dict_field(dict, "params", ctx)? {
                Some(params_val) => {
                    match params_val {
                        Value::Dict(params_dict) => {
                            // Extract params from integer-keyed dict
                            let mut param_names = Vec::new();
                            let mut i = 0i64;
                            while let Some(thunk_id) = params_dict.get(&Key::Int(i)) {
                                let thunk = ctx.get_thunk(*thunk_id);
                                let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                                    message: format!("param {} is not materialized", i),
                                    field_path: vec!["params".to_string(), i.to_string()],
                                })?;
                                match val {
                                    Value::String {
                                        ref source,
                                        start,
                                        end,
                                    } => param_names.push(source[start..end].to_string()),
                                    _ => {
                                        return Err(AstError {
                                            message: format!("param {} must be String", i),
                                            field_path: vec!["params".to_string(), i.to_string()],
                                        });
                                    }
                                }
                                i += 1;
                            }
                            param_names
                        }
                        _ => {
                            return Err(AstError {
                                message: "params must be a Dict".to_string(),
                                field_path: vec!["params".to_string()],
                            });
                        }
                    }
                }
                None => Vec::new(),
            };

            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::TypeAlias {
                params,
                body: Box::new(dict_to_ast(&expr_val, ctx)?),
            }
        }

        "type-assert" | "TypeAssert" => {
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

        "annotated" | "Annotated" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let annotation_val = get_dict_field(dict, "annotation", &["type"], ctx)?;
            let annotation = dict_to_annotation(&annotation_val, &["annotation"], ctx)?;

            Expr::Annotated { name, annotation }
        }

        "rest" | "Rest" => {
            let name_val = get_field(dict, "name", &["type"], ctx)?;
            let name_opt = match name_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => Some(source[start..end].to_string()),
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

        "quote" | "Quote" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::Quote(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "unquote" | "Unquote" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::Unquote(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "unquote-splice" | "UnquoteSplice" => {
            let expr_val = get_dict_field(dict, "expr", &["type"], ctx)?;
            Expr::UnquoteSplice(Box::new(dict_to_ast(&expr_val, ctx)?))
        }

        "defmacro" | "DefMacro" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let params_val = get_dict_field(dict, "params", &["type"], ctx)?;
            let body_val = get_dict_field(dict, "body", &["type"], ctx)?;

            Expr::DefMacro {
                name,
                params: Rc::new(dict_to_ast(&params_val, ctx)?),
                body: Rc::new(dict_to_ast(&body_val, ctx)?),
            }
        }

        "macro-decl" | "MacroDecl" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let params_val = get_dict_field(dict, "params", &["type"], ctx)?;
            let body_val = get_dict_field(dict, "body", &["type"], ctx)?;
            Expr::MacroDecl {
                name,
                params: Box::new(dict_to_ast(&params_val, ctx)?),
                body: Box::new(dict_to_ast(&body_val, ctx)?),
            }
        }

        "splice" | "Splice" => {
            let forms_val = get_dict_field(dict, "forms", &["type"], ctx)?;
            let forms_list = extract_list(&forms_val, &["forms"], ctx)?;
            let mut forms = Vec::new();
            for form_val in forms_list.into_iter() {
                forms.push(dict_to_ast(&form_val, ctx)?);
            }
            Expr::Splice(forms)
        }

        "syntax-class" | "SyntaxClass" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            let pattern_val = get_dict_field(dict, "pattern", &["type"], ctx)?;
            // message is optional — emitter omits the key when None
            let message = if dict.contains_key(&Key::String("message".into())) {
                let message_val = get_field(dict, "message", &["type"], ctx)?;
                match message_val {
                    Value::String {
                        ref source,
                        start,
                        end,
                    } => Some(source[start..end].to_string()),
                    Value::Dict(d) if d.is_empty() => None,
                    _ => {
                        return Err(AstError {
                            message: "message must be String or empty Dict".into(),
                            field_path: vec!["message".into()],
                        })
                    }
                }
            } else {
                None
            };
            Expr::SyntaxClass {
                name,
                pattern: Box::new(dict_to_ast(&pattern_val, ctx)?),
                message,
            }
        }

        "type-app" | "TypeApp" => {
            let func_val = get_dict_field(dict, "func", &["type"], ctx)?;
            let arg_val = get_dict_field(dict, "arg", &["type"], ctx)?;
            Expr::TypeApp {
                func: Box::new(dict_to_ast(&func_val, ctx)?),
                arg: Box::new(dict_to_ast(&arg_val, ctx)?),
            }
        }

        "let" | "LetDecl" => {
            let bindings_val = get_dict_field(dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list.into_iter() {
                let binding = dict_to_ast(&binding_val, ctx)?;
                bindings.push(binding);
            }
            Expr::LetDecl { bindings }
        }

        "case" | "CaseArm" => {
            let pattern_val = get_dict_field(dict, "pattern", &["type"], ctx)?;
            let body_val = get_dict_field(dict, "body", &["type"], ctx)?;
            Expr::CaseArm {
                pattern: Box::new(dict_to_ast(&pattern_val, ctx)?),
                body: Box::new(dict_to_ast(&body_val, ctx)?),
            }
        }

        // ---- Gap fills: variants emitted by expr_to_thunk_id but previously missing here ----

        "match" | "Match" => {
            let scrutinee_val = get_dict_field(dict, "scrutinee", &["type"], ctx)?;
            let scrutinee = Box::new(dict_to_ast(&scrutinee_val, ctx)?);
            let arms_val = get_dict_field(dict, "arms", &["type"], ctx)?;
            let arms_list = extract_list(&arms_val, &["arms"], ctx)?;
            let mut arms = Vec::new();
            for arm_val in arms_list {
                let arm_dict = match &arm_val {
                    Value::Dict(d) => d.clone(),
                    _ => {
                        return Err(AstError {
                            message: "match arm must be Dict".into(),
                            field_path: vec!["arms".into()],
                        })
                    }
                };
                let pattern_val = get_dict_field(&arm_dict, "pattern", &["arms"], ctx)?;
                let pattern_spanned = {
                    // pattern_to_thunk_id emits a plain Dict (not a Variant) with a
                    // "type" field. Reconstruct the Pattern using dict_to_pattern.
                    let pat_dict = match &pattern_val {
                        Value::Dict(d) => d,
                        _ => {
                            return Err(AstError {
                                message: "pattern must be Dict".into(),
                                field_path: vec!["arms".into(), "pattern".into()],
                            })
                        }
                    };
                    let pat_span = extract_span(pat_dict, ctx).unwrap_or_else(Span::origin);
                    let pat = dict_to_pattern(pat_dict, &["arms", "pattern"], ctx)?;
                    Spanned::new(pat, pat_span)
                };
                let guard = if arm_dict.contains_key(&Key::String("guard".into())) {
                    let guard_val = get_dict_field(&arm_dict, "guard", &["arms"], ctx)?;
                    Some(Box::new(dict_to_ast(&guard_val, ctx)?))
                } else {
                    None
                };
                let body_val = get_dict_field(&arm_dict, "body", &["arms"], ctx)?;
                let body = Box::new(dict_to_ast(&body_val, ctx)?);
                arms.push(crate::ast::MatchArm {
                    pattern: pattern_spanned,
                    guard,
                    body,
                });
            }
            Expr::Match { scrutinee, arms }
        }

        "class-decl" | "ClassDecl" => {
            let name = get_string_field(dict, "name", &["type"], ctx)?;
            // params: integer-keyed dict of param name strings
            let params = match get_optional_dict_field(dict, "params", ctx)? {
                Some(Value::Dict(params_dict)) => {
                    let mut param_names = Vec::new();
                    let mut i = 0i64;
                    while let Some(thunk_id) = params_dict.get(&Key::Int(i)) {
                        let thunk = ctx.get_thunk(*thunk_id);
                        let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                            message: format!("ClassDecl param {} is not materialized", i),
                            field_path: vec!["params".to_string(), i.to_string()],
                        })?;
                        match val {
                            Value::String { ref source, start, end } => {
                                param_names.push(source[start..end].to_string())
                            }
                            _ => {
                                return Err(AstError {
                                    message: format!("ClassDecl param {} must be String", i),
                                    field_path: vec!["params".to_string(), i.to_string()],
                                });
                            }
                        }
                        i += 1;
                    }
                    param_names
                }
                _ => Vec::new(),
            };
            // methods: string-keyed dict — reconstruct as Vec<Spanned<Entry>>
            let methods = match get_optional_dict_field(dict, "methods", ctx)? {
                Some(Value::Dict(methods_dict)) => {
                    let mut method_entries = Vec::new();
                    for (key, thunk_id) in &methods_dict {
                        if let Key::String(method_name) = key {
                            let thunk = ctx.get_thunk(*thunk_id);
                            let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                                message: format!(
                                    "ClassDecl method {} value is not materialized",
                                    method_name
                                ),
                                field_path: vec!["methods".to_string()],
                            })?;
                            let value_expr = dict_to_ast(&val, ctx)?;
                            let entry_span = value_expr.span;
                            let key_expr =
                                Spanned::new(Expr::Str(method_name.to_string()), entry_span);
                            method_entries.push(Spanned::new(
                                crate::ast::Entry {
                                    key: Some(key_expr),
                                    value: std::rc::Rc::new(value_expr),
                                },
                                entry_span,
                            ));
                        }
                    }
                    method_entries
                }
                _ => Vec::new(),
            };
            // determines: integer-keyed dict of expression dicts
            let determines = match get_optional_dict_field(dict, "determines", ctx)? {
                Some(val) => {
                    let determines_list = extract_list(&val, &["determines"], ctx)?;
                    let mut result = Vec::new();
                    for det_val in determines_list {
                        result.push(dict_to_ast(&det_val, ctx)?);
                    }
                    result
                }
                None => Vec::new(),
            };
            // resolver: optional expression dict (Expr::ClassDecl.resolver is Box<Spanned<Expr>>)
            let resolver = match get_optional_dict_field(dict, "resolver", ctx)? {
                Some(res_val) => Some(Box::new(dict_to_ast(&res_val, ctx)?)),
                None => None,
            };
            // injective: optional bool (absent = false)
            let resolver_injective =
                get_optional_dict_field(dict, "injective", ctx)?.map_or(false, |v| {
                    matches!(v, Value::Bool(true))
                });
            Expr::ClassDecl {
                name,
                params,
                superclasses: Vec::new(), // not serialized — see grammar-doc-polish TODO
                methods,
                determines,
                resolver,
                resolver_injective,
            }
        }

        "instance-decl" | "InstanceDecl" => {
            let class_name = get_string_field(dict, "class", &["type"], ctx)?;
            let arms_val = get_dict_field(dict, "arms", &["type"], ctx)?;
            let arms_list = extract_list(&arms_val, &["arms"], ctx)?;
            let mut arms = Vec::new();
            for arm_val in arms_list {
                let arm_dict = match &arm_val {
                    Value::Dict(d) => d.clone(),
                    _ => {
                        return Err(AstError {
                            message: "instance arm must be Dict".into(),
                            field_path: vec!["arms".into()],
                        })
                    }
                };
                let pattern_val = get_dict_field(&arm_dict, "pattern", &["arms"], ctx)?;
                let pattern_expr = dict_to_ast(&pattern_val, ctx)?;
                // methods: string-keyed dict — reconstruct as Vec<Spanned<Entry>>
                let methods = match get_optional_dict_field(&arm_dict, "methods", ctx)? {
                    Some(Value::Dict(methods_dict)) => {
                        let mut method_entries = Vec::new();
                        for (key, thunk_id) in &methods_dict {
                            if let Key::String(method_name) = key {
                                let thunk = ctx.get_thunk(*thunk_id);
                                let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                                    message: format!(
                                        "InstanceDecl method {} value is not materialized",
                                        method_name
                                    ),
                                    field_path: vec!["arms".to_string(), "methods".to_string()],
                                })?;
                                let value_expr = dict_to_ast(&val, ctx)?;
                                let entry_span = value_expr.span;
                                let key_expr =
                                    Spanned::new(Expr::Str(method_name.to_string()), entry_span);
                                method_entries.push(Spanned::new(
                                    crate::ast::Entry {
                                        key: Some(key_expr),
                                        value: std::rc::Rc::new(value_expr),
                                    },
                                    entry_span,
                                ));
                            }
                        }
                        method_entries
                    }
                    _ => Vec::new(),
                };
                arms.push((pattern_expr, methods));
            }
            Expr::InstanceDecl { class_name, arms }
        }

        "pattern-decl" | "PatternDecl" => {
            let bindings_val = get_dict_field(dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list {
                bindings.push(dict_to_ast(&binding_val, ctx)?);
            }
            Expr::PatternDecl { bindings }
        }

        "placeholder" | "Placeholder" => Expr::Placeholder,

        "error" | "AstError" => {
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<String, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(source[start..end].to_string()),
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Dict(_) | Value::Variant { .. } => Ok(val),
        _ => Err(AstError {
            message: format!("field '{}' must be Dict or Variant", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_optional_dict_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    ctx: &Arc<crate::eval::EvalContext>,
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

fn extract_span(
    dict: &IndexMap<Key, ThunkId>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Span> {
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

fn extract_position(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Option<Position> {
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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
    ctx: &Arc<crate::eval::EvalContext>,
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

/// Deserialize a `Pattern` from the dict schema emitted by `pattern_to_thunk_id`.
///
/// Patterns are encoded as plain `Value::Dict` (not Variant) with a `type:` field.
/// Called from the `"Match"` arm of `dict_to_ast_from_dict`.
fn dict_to_pattern(
    dict: &IndexMap<Key, ThunkId>,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<crate::ast::Pattern, AstError> {
    use crate::ast::{LiteralPattern, Pattern};
    let type_str = get_string_field(dict, "type", path, ctx)?;
    match type_str.as_str() {
        "wildcard" => Ok(Pattern::Wildcard),
        "variable" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Ok(Pattern::Variable(name))
        }
        "type_tag" => {
            let tag = get_string_field(dict, "tag", path, ctx)?;
            Ok(Pattern::TypeTag(tag))
        }
        "pin" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Ok(Pattern::Pin(name))
        }
        "literal" => {
            let val = get_field(dict, "value", path, ctx)?;
            let lit = match val {
                Value::Int(n) => LiteralPattern::Int(n),
                Value::Float(f) => LiteralPattern::Float(f),
                Value::Bool(b) => LiteralPattern::Bool(b),
                Value::String { ref source, start, end } => {
                    LiteralPattern::Str(source[start..end].to_string())
                }
                _ => {
                    return Err(AstError {
                        message: "literal pattern value must be Int, Float, Bool, or String".into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            Ok(Pattern::Literal(lit))
        }
        "dict" => {
            let fields_val = get_field(dict, "fields", path, ctx)?;
            let fields_dict = match fields_val {
                Value::Dict(d) => d,
                _ => {
                    return Err(AstError {
                        message: "dict pattern fields must be Dict".into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            let rest = get_bool_field(dict, "rest", path, ctx)?;
            // Iterate integer-keyed fields in order
            let mut fields = Vec::new();
            let mut i = 0i64;
            while let Some(field_thunk_id) = fields_dict.get(&Key::String(Rc::from(i.to_string().as_str()))) {
                let field_thunk = ctx.get_thunk(*field_thunk_id);
                let field_val = field_thunk.try_get_materialized().ok_or_else(|| AstError {
                    message: format!("dict pattern field {} is not materialized", i),
                    field_path: path.iter().map(|s| s.to_string()).collect(),
                })?;
                let field_dict = match field_val {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: "dict pattern field must be Dict".into(),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })
                    }
                };
                let key = get_string_field(&field_dict, "key", path, ctx)?;
                let pattern_val_inner = get_field(&field_dict, "pattern", path, ctx)?;
                let inner_dict = match pattern_val_inner {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: "dict pattern field pattern must be Dict".into(),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })
                    }
                };
                let inner_span =
                    extract_span(&inner_dict, ctx).unwrap_or_else(Span::origin);
                let inner_pat = dict_to_pattern(&inner_dict, path, ctx)?;
                fields.push((key, Spanned::new(inner_pat, inner_span)));
                i += 1;
            }
            Ok(Pattern::Dict { fields, rest })
        }
        "seq" => {
            let head_val = get_field(dict, "head", path, ctx)?;
            let head_dict = match head_val {
                Value::Dict(d) => d,
                _ => {
                    return Err(AstError {
                        message: "seq head must be Dict".into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            let head_span = extract_span(&head_dict, ctx).unwrap_or_else(Span::origin);
            let head_pat = dict_to_pattern(&head_dict, path, ctx)?;

            let tail_val = get_field(dict, "tail", path, ctx)?;
            let tail_dict = match tail_val {
                Value::Dict(d) => d,
                _ => {
                    return Err(AstError {
                        message: "seq tail must be Dict".into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            let tail_span = extract_span(&tail_dict, ctx).unwrap_or_else(Span::origin);
            let tail_pat = dict_to_pattern(&tail_dict, path, ctx)?;

            Ok(Pattern::Seq {
                head: Box::new(Spanned::new(head_pat, head_span)),
                tail: Box::new(Spanned::new(tail_pat, tail_span)),
            })
        }
        "constructor" => {
            let tag = get_string_field(dict, "tag", path, ctx)?;
            let binding = if dict.contains_key(&Key::String("binding".into())) {
                let binding_val = get_field(dict, "binding", path, ctx)?;
                let binding_dict = match binding_val {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: "constructor binding must be Dict".into(),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })
                    }
                };
                let binding_span = extract_span(&binding_dict, ctx).unwrap_or_else(Span::origin);
                let binding_pat = dict_to_pattern(&binding_dict, path, ctx)?;
                Some(Box::new(Spanned::new(binding_pat, binding_span)))
            } else {
                None
            };
            Ok(Pattern::Constructor { tag, binding })
        }
        "or" => {
            let patterns_val = get_field(dict, "patterns", path, ctx)?;
            let patterns_dict = match patterns_val {
                Value::Dict(d) => d,
                _ => {
                    return Err(AstError {
                        message: "or pattern patterns must be Dict".into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            let mut patterns = Vec::new();
            let mut i = 0i64;
            while let Some(pat_thunk_id) = patterns_dict.get(&Key::Int(i)) {
                let pat_thunk = ctx.get_thunk(*pat_thunk_id);
                let pat_val = pat_thunk.try_get_materialized().ok_or_else(|| AstError {
                    message: format!("or pattern element {} is not materialized", i),
                    field_path: path.iter().map(|s| s.to_string()).collect(),
                })?;
                let pat_dict = match pat_val {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: "or pattern element must be Dict".into(),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })
                    }
                };
                let pat_span = extract_span(&pat_dict, ctx).unwrap_or_else(Span::origin);
                let pat = dict_to_pattern(&pat_dict, path, ctx)?;
                patterns.push(Spanned::new(pat, pat_span));
                i += 1;
            }
            Ok(Pattern::Or(patterns))
        }
        _ => Err(AstError {
            message: format!("unknown pattern type: {}", type_str),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

/// Converts a file-schema dict back to a `File` AST.
///
/// Reconstructs a `File` struct from the schema emitted by `surface_program_to_dict`:
/// - `documents`: list of document dicts, each containing:
///   - `expressions`: seq of expression AST dicts (converted via `dict_to_ast`)
///   - `name`: string or empty dict
///   - `stage`: `[Runtime]` or `[Type]` nominal variant
///   - `output-type`, `expects`: annotation dicts or empty dict
/// - `caps`: always `None` (not serialized)
/// - Spans recovered from each node's `span:` field via `extract_span`
///
/// Used internally by `dict_to_surface_program` via the File bridge.
///
/// **Root type constraint**: `dict_to_file` requires a `Value::Dict` root (the full file schema).
/// Unlike `dict_to_ast` which accepts both `Dict` and `Variant`, the file root must be a `Dict`
/// with a `documents` field. This asymmetry is correct: `surface_program_to_dict` always emits
/// `Dict` for file roots, never `Variant`, so the round-trip is consistent.
///
/// # Visibility
/// Not part of the public API — callers should use `dict_to_surface_program` instead.
/// Retained as the implementation backing `dict_to_surface_program` and for `#[cfg(test)]` usage.
#[doc(hidden)]
fn dict_to_file(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Result<File, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "file must be Dict".into(),
                field_path: vec![],
            })
        }
    };

    // Get the documents list
    let documents_val = get_dict_field(dict, "documents", &[], ctx)?;
    let documents_list = extract_list(&documents_val, &["documents"], ctx)?;

    let mut documents = Vec::new();
    for (i, doc_val) in documents_list.into_iter().enumerate() {
        let i_str = i.to_string();
        let doc_dict = match &doc_val {
            Value::Dict(d) => d,
            _ => {
                return Err(AstError {
                    message: "document must be Dict".into(),
                    field_path: vec!["documents".to_string(), i_str],
                })
            }
        };

        // Extract expressions
        let exprs_val = get_dict_field(doc_dict, "expressions", &["documents", &i_str], ctx)?;
        let exprs_list = extract_list(&exprs_val, &["documents", &i_str, "expressions"], ctx)?;
        let mut expressions = Vec::new();
        for expr_val in exprs_list.into_iter() {
            let expr = dict_to_ast(&expr_val, ctx)?;
            expressions.push(Rc::new(expr));
        }

        // Extract name (string or None if empty dict)
        let name_val = get_field(doc_dict, "name", &["documents", &i_str], ctx)?;
        let name = match &name_val {
            Value::String { source, start, end } => Some(source[*start..*end].to_string()),
            Value::Dict(d) if d.is_empty() => None,
            _ => {
                return Err(AstError {
                    message: "name must be String or empty Dict".into(),
                    field_path: vec!["documents".to_string(), i_str, "name".to_string()],
                })
            }
        };

        // Extract stage (nominal variant: [Runtime] or [Type])
        let stage_val = get_dict_field(doc_dict, "stage", &["documents", &i_str], ctx)?;
        let stage = match &stage_val {
            Value::Variant { tag, payload: None } => match tag.as_str() {
                "Runtime" => Some(Stage::Runtime),
                "Type" => Some(Stage::Type),
                _ => {
                    return Err(AstError {
                        message: format!("unknown stage variant: {}", tag),
                        field_path: vec!["documents".to_string(), i_str, "stage".to_string()],
                    })
                }
            },
            _ => {
                return Err(AstError {
                    message: "stage must be Variant with no payload".into(),
                    field_path: vec!["documents".to_string(), i_str, "stage".to_string()],
                })
            }
        };

        // Extract output-type (annotation or None if empty dict)
        let output_type_val = get_dict_field(doc_dict, "output-type", &["documents", &i_str], ctx)?;
        let output_type = match &output_type_val {
            Value::Dict(d) if d.is_empty() => None,
            _ => Some(dict_to_annotation(
                &output_type_val,
                &["documents", &i_str, "output-type"],
                ctx,
            )?),
        };

        // Extract expects (annotation or None if empty dict)
        let expects_val = get_dict_field(doc_dict, "expects", &["documents", &i_str], ctx)?;
        let expects = match &expects_val {
            Value::Dict(d) if d.is_empty() => None,
            _ => Some(dict_to_annotation(
                &expects_val,
                &["documents", &i_str, "expects"],
                ctx,
            )?),
        };

        // caps is always None (not serialized by document_to_dict)
        let caps = None;

        // Extract span
        let span = extract_span(doc_dict, ctx).unwrap_or_else(Span::origin);

        documents.push(Spanned::new(
            Document {
                expressions,
                name,
                output_type,
                expects,
                caps,
                stage,
            },
            span,
        ));
    }

    Ok(File { documents })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        use crate::value::Environment;
        use std::sync::RwLock;

        let env = Arc::new(RwLock::new(Environment::new()));
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Peel a `Value::Variant` to its payload dict.
    /// Panics with a helpful message if the value is not a Variant with a Dict payload.
    fn peel_variant(
        val: Value,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> (String, IndexMap<Key, ThunkId>) {
        match val {
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } => {
                let payload_thunk = ctx.get_thunk(payload_id);
                match payload_thunk.try_get_materialized() {
                    Some(Value::Dict(map)) => (tag, map),
                    other => panic!("expected Dict payload for Variant, got {:?}", other),
                }
            }
            other => panic!("expected Variant, got {:?}", other),
        }
    }

    #[test]
    fn test_ast_to_dict_int() {
        let expr = sp(Expr::Int(42));
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = ast_to_dict_expr(&expr, &opts, &ctx).unwrap();

        let outer = thunk
            .try_get_materialized()
            .expect("thunk not materialized");
        let (tag, map) = peel_variant(outer, &ctx);
        assert_eq!(tag, "Literal");

        // Check kind field
        let kind_id = map.get(&Key::String("kind".into())).unwrap();
        let kind_thunk = ctx.get_thunk(*kind_id);
        assert_eq!(kind_thunk.try_get_materialized(), Some(string_val("int")));

        // Check value field
        let value_id = map.get(&Key::String("value".into())).unwrap();
        let value_thunk = ctx.get_thunk(*value_id);
        assert_eq!(value_thunk.try_get_materialized(), Some(Value::Int(42)));
    }

    #[test]
    fn test_ast_to_dict_var() {
        let expr = sp(Expr::var_ref("x".into()));
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = ast_to_dict_expr(&expr, &opts, &ctx).unwrap();

        let outer = thunk
            .try_get_materialized()
            .expect("thunk not materialized");
        let (tag, map) = peel_variant(outer, &ctx);
        assert_eq!(tag, "VarRef");

        let name_id = map.get(&Key::String("name".into())).unwrap();
        let name_thunk = ctx.get_thunk(*name_id);
        assert_eq!(name_thunk.try_get_materialized(), Some(string_val("x")));
    }

    #[test]
    fn test_surface_program_to_dict_file_schema_version() {
        use crate::parser::parse;

        let parse_output = parse("1").unwrap();
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        match thunk.try_get_materialized() {
            Some(Value::Dict(map)) => {
                let type_id = map.get(&Key::String("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(type_thunk.try_get_materialized(), Some(string_val("file")));

                let version_id = map.get(&Key::String("schema-version".into())).unwrap();
                let version_thunk = ctx.get_thunk(*version_id);
                assert_eq!(version_thunk.try_get_materialized(), Some(Value::Int(1)));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_bare_flag_on_bare_word_strings() {
        use crate::parser::parse;

        // Parse "[foo: 1]" — the key "foo" should have bare: true
        let input = "[foo: 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

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
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
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
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            // Check bare: true
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(true)),
                                                                "bare should be true for bare word 'foo'"
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
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
        use crate::parser::parse;

        // Parse "[\"foo\": 1]" — the key "foo" should have bare: false
        let input = "[\"foo\": 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

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
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
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
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(false)),
                                                                "bare should be false for quoted string \"foo\""
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
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
        use crate::parser::parse;

        // Parse "[# comment\nx: 1]" — the entry should have leading-comments: [" comment"]
        let input = "[# comment\nx: 1]";
        let parse_output = parse(input).unwrap();
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

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

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
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
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
                                                                        Some(string_val(" comment")),
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
        use crate::parser::parse;
        use std::collections::BTreeMap;

        // Manually inject blank-before data to test the ast_dict lookup.
        // The parser's main loop does not track blank lines between dict entries
        // (skip_whitespace_tokens handles that in specific call sites), so we
        // construct the blank_before map by hand.
        let input = "[a: 1\nb: 2]";
        let parse_output = parse(input).unwrap();

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

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

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
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
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
                                                                .expect(
                                                                    "blank-before field missing",
                                                                );
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
        use crate::parser::parse;

        // Parse "[foo: 1]" with both source and comments None
        let input = "[foo: 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: None,
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

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
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
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
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(false)),
                                                                "bare should be false when source is None"
                                                            );

                                                            // Check that blank-before is still present (always included)
                                                            let blank_id = entry_dict
                                                                .get(&Key::String(
                                                                    "blank-before".into(),
                                                                ))
                                                                .expect(
                                                                    "blank-before field missing",
                                                                );
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
