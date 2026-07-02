//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `SurfaceNode` — all cross-phase data lives inline on nodes.
//! De Bruijn coordinates are read from the inline `resolution` field on VarRef/DotAccess nodes.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `Error` (unresolvable — genuine compile error)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (with resolved_type from the inline TypeAnnotation field or Type::Unknown)
//! - `TypeAssertPending` in patterns → `TypeAssert` (using the inline `resolved` TypeAnnotation field)
//! - `DotAccess` with `field_slot` set → `Call(slot-get, [Int(slot), target])` (O(1) positional access)
//! - `DotAccess` without `field_slot` → `Call(field-get, [Str/Int(key), target])` (key-based lookup)
//! - `SurfaceNode.type_guard` set → wraps the lowered CoreExpr in `CoreExpr::TypeAssert`
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::Arc;

use crate::ast::{
    CoreEntry, CoreExpr, CoreMatchArm, CoreNamedArg, CoreParam, Pattern, Spanned, SurfaceEntry,
    SurfaceExpression, SurfaceNode,
};
use crate::rust_span;

/// Lower a single surface node to a CoreExpr.
///
/// This is the entry point for per-thunk lowering. Called from `eval_materialize.rs`
/// when a `UnevaluatedState::Surface` thunk is first forced.
///
/// Lowering errors (malformed AST, impossible variant combinations) propagate as
/// `CoreExpr::Error` — consistent with tinct's lazy semantics where errors surface
/// at force time, not at construction time.
///
/// All cross-phase data (type annotations, field slots, provenance) is read from inline
/// fields on the AST nodes — no external tables are consulted.
pub fn lower(arc: &Arc<SurfaceNode>) -> Spanned<CoreExpr> {
    let span = arc.span.clone();
    let core_expr = lower_expr(arc, &arc.expr);

    // Apply type guard if the type checker set one on this node.
    let core_expr = if let Some(guard_type) = arc.type_guard.get() {
        CoreExpr::TypeAssert {
            annotation: crate::ast::Spanned::new(
                crate::ast::Annotation::Simple("__guard__".to_string()),
                span.clone(),
            ),
            expr: Arc::new(crate::ast::Spanned::new(core_expr, span.clone())),
            resolved_type: guard_type.clone(),
            pipeline_blame: None,
        }
    } else {
        core_expr
    };

    Spanned::new(core_expr, span)
}

/// Resolve an annotation name to a Type for TypeAssertPending pattern lowering.
///
/// Mirrors typecheck_annot.rs::resolve_type_name for the builtin type names prelude
/// uses in [@Type _]: patterns. Used when the inline `resolved` TypeAnnotation has no
/// entry (which is always the case currently, as populate is not yet wired up).
/// Unknown is the accept-all fallback for unrecognized names (--no-typecheck, macros).
pub(crate) fn annotation_name_to_type(name: &str) -> crate::type_def::Type {
    use crate::type_def::{Row, RowTail, Type};
    match name {
        "Int" => Type::Int,
        "Float" => Type::Float,
        "String" | "Str" => Type::Str,
        "Bytes" => Type::Bytes,
        "Proxy" => Type::Proxy,
        // Empty record = "any dict" under BAS width subtyping.
        "Dict" | "Record" | "Null" => Type::Record(Row {
            fields: indexmap::IndexMap::new(),
            tail: RowTail::Empty,
        }),
        // Variadic 0-required-param function = any callable (Function or Builtin).
        "Fn" | "Function" | "Builtin" => Type::Function {
            params: vec![],
            ret: Box::new(Type::Any),
            variadic: true,
            required_count: 0,
        },
        // Named types: look up via TyCon for Boolean, Seq, etc.
        "Bool" | "Boolean" => Type::TyCon("Boolean".to_string()),
        "Seq" => Type::TyCon("Seq".to_string()),
        _ => Type::Unknown,
    }
}

/// Lower a `Pattern`, converting `TypeAssertPending → TypeAssert`.
///
/// TypeAssertPending is ALWAYS converted to TypeAssert — never left as-is.
/// The inline `resolved` TypeAnnotation field is checked first (set by the type checker).
/// If not set, `annotation_name_to_type` provides a direct name→Type mapping.
/// Unknown is the fallback for unrecognized names (accept-all).
///
/// Recursively walks all sub-patterns so nested TypeAssertPending nodes are
/// also converted (e.g., inside Or, Dict, Seq, Constructor bindings).
fn lower_pattern(pat: &Pattern) -> Pattern {
    match pat {
        Pattern::TypeAssertPending {
            annotation,
            inner,
            resolved,
        } => {
            // Read the inline resolved type — set by the type checker, or fall back to name→Type.
            let resolved_type = resolved.get().cloned().unwrap_or_else(|| {
                if let crate::ast::Annotation::Simple(name) = &annotation.node {
                    annotation_name_to_type(name)
                } else {
                    crate::type_def::Type::Unknown
                }
            });
            let lowered_inner = inner.as_ref().map(|boxed| {
                Box::new(Spanned::new(lower_pattern(&boxed.node), boxed.span.clone()))
            });
            Pattern::TypeAssert {
                resolved_type,
                inner: lowered_inner,
            }
        }

        Pattern::TypeAssert {
            resolved_type,
            inner,
        } => {
            // Already elaborated — recurse into inner.
            let elaborated_inner = inner.as_ref().map(|boxed| {
                Box::new(Spanned::new(lower_pattern(&boxed.node), boxed.span.clone()))
            });
            Pattern::TypeAssert {
                resolved_type: resolved_type.clone(),
                inner: elaborated_inner,
            }
        }

        Pattern::Or(branches) => Pattern::Or(
            branches
                .iter()
                .map(|b| Spanned::new(lower_pattern(&b.node), b.span.clone()))
                .collect(),
        ),

        Pattern::Constructor { tag, binding } => Pattern::Constructor {
            tag: tag.clone(),
            binding: binding
                .as_ref()
                .map(|b| Box::new(Spanned::new(lower_pattern(&b.node), b.span.clone()))),
        },

        Pattern::Dict { fields, rest } => Pattern::Dict {
            fields: fields
                .iter()
                .map(|(k, s)| {
                    (
                        k.clone(),
                        Spanned::new(lower_pattern(&s.node), s.span.clone()),
                    )
                })
                .collect(),
            rest: *rest,
        },

        // Leaf patterns: no sub-patterns to lower.
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Pin(..) => pat.clone(),

        // T-1140: Predicate patterns carry a SurfaceNode — passed through unchanged.
        // The SurfaceNode is lowered on demand inside MatchDispatch at eval time.
        Pattern::Predicate(_) => pat.clone(),
    }
}

fn lower_expr(arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int(*n),
        SurfaceExpression::U64(n) => CoreExpr::U64(*n),
        SurfaceExpression::Float(n) => CoreExpr::Float(*n),
        SurfaceExpression::Str(s) => CoreExpr::Str(s.clone()),

        SurfaceExpression::VarRef {
            name, resolution, ..
        } => {
            match resolution.get() {
                Some(Some((level, slot))) => CoreExpr::Var {
                    name: name.clone(),
                    level,
                    slot,
                },
                _ => {
                    // Some(None) = unresolvable; None = resolver never ran.
                    // Both are genuine compile errors — produce Error so the evaluator
                    // surfaces it at force time.
                    CoreExpr::Error(arc.span.clone())
                }
            }
        }

        SurfaceExpression::DotAccess {
            expr: Some(inner),
            field,
            field_slot,
            resolution,
        } => {
            // Get the root scope level from the resolver-written resolution field.
            // The resolver writes the (level, slot) of "field-get" (slot 0 in root scope).
            // slot-get lives at slot 1 in the same root scope (same level, slot 1).
            let root_level = match resolution.get() {
                Some(Some((level, _slot))) => level,
                _ => {
                    // Resolver did not find "field-get" in scope — emit Error.
                    return CoreExpr::Error(arc.span.clone());
                }
            };

            // Build the getter function Var and the key argument.
            let (getter_root_slot, key_arg) = if let Some(slot) = field_slot.get() {
                // Typed: use slot-get (positional O(1) access) at root scope slot 1.
                (
                    crate::builtins_core::SLOT_GET_ROOT_SLOT,
                    CoreExpr::Int(slot as i64),
                )
            } else {
                // Untyped: use field-get (key-based lookup) at root scope slot 0.
                let key_core = match field {
                    crate::ast::DotKey::Int(n) => CoreExpr::Int(*n),
                    crate::ast::DotKey::Ident(s) => CoreExpr::Str(s.clone()),
                };
                (crate::builtins_core::FIELD_GET_ROOT_SLOT, key_core)
            };

            let getter_name = if getter_root_slot == crate::builtins_core::FIELD_GET_ROOT_SLOT {
                "field-get"
            } else {
                "slot-get"
            };

            let getter_var = Arc::new(crate::ast::Spanned::new(
                CoreExpr::Var {
                    name: getter_name.to_string(),
                    level: root_level,
                    slot: getter_root_slot,
                },
                arc.span.clone(),
            ));
            let key_node = Arc::new(crate::ast::Spanned::new(key_arg, arc.span.clone()));
            let target_node = Arc::new(lower(inner));

            CoreExpr::Call {
                func: getter_var,
                args: vec![key_node, target_node],
                named_args: vec![],
                implied: true,
            }
        }

        // Leading-dot form: `.name` with no preceding expression.
        // The resolver has written parent-scope coordinates into the node's `resolution` field.
        // Read them directly — the lowered result is indistinguishable from a normal variable reference.
        SurfaceExpression::DotAccess {
            expr: None,
            field: crate::ast::DotKey::Ident(name),
            resolution,
            ..
        } => match resolution.get() {
            Some(Some((level, slot))) => CoreExpr::Var {
                name: name.clone(),
                level,
                slot,
            },
            _ => CoreExpr::Error(arc.span.clone()),
        },

        // Leading-dot with integer key: `.0` — no parent-scope numeric lookup. The parser
        // rejects this at parse time, so this is a safety fallback only.
        SurfaceExpression::DotAccess {
            expr: None,
            field: crate::ast::DotKey::Int(_),
            ..
        } => CoreExpr::Error(arc.span.clone()),

        // Pipe is syntactic sugar — rewrite to Call(rhs, [lhs]) so the evaluator
        // sees only Call nodes. Equivalent to: f |> g  ==  g(f).
        SurfaceExpression::Pipe { lhs, rhs } => CoreExpr::Call {
            func: Arc::new(lower(rhs)),
            args: vec![Arc::new(lower(lhs))],
            named_args: vec![],
            implied: true,
        },

        SurfaceExpression::Sequential(exprs) => {
            CoreExpr::Sequential(exprs.iter().map(|e| Arc::new(lower(e))).collect())
        }

        SurfaceExpression::Dict(entries) => {
            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            for se in entries {
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::InstanceDecl { .. } => {
                            if se.node.key.is_some() {
                                let lowered = lower_expr(&se.node.value, &se.node.value.expr);
                                let key = se.node.key.as_ref().map(|k| Arc::new(lower(k)));
                                let value = Arc::new(Spanned::new(lowered, se.span.clone()));
                                core_entries
                                    .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                            }
                        }
                        crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                            let type_name_opt = extract_type_name_from_key(&se.node.key);
                            let ctor_dict =
                                lower_type_alias_to_constructor_dict(type_name_opt, body);
                            let key = se.node.key.as_ref().map(|k| {
                                let lowered = match &k.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        CoreExpr::Str(name.clone())
                                    }
                                    SurfaceExpression::Annotated { name, .. } => {
                                        CoreExpr::Str(name.clone())
                                    }
                                    _ => lower_expr(k, &k.expr),
                                };
                                Arc::new(Spanned::new(lowered, k.span.clone()))
                            });
                            let value = Arc::new(Spanned::new(ctor_dict, se.span.clone()));
                            core_entries
                                .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                        }
                        _ => {
                            continue;
                        }
                    }
                } else {
                    let key = se.node.key.as_ref().map(|k| {
                        let lowered = match &k.expr {
                            SurfaceExpression::VarRef { name, .. } => CoreExpr::Str(name.clone()),
                            SurfaceExpression::Annotated { name, .. } => {
                                CoreExpr::Str(name.clone())
                            }
                            _ => lower_expr(k, &k.expr),
                        };
                        Arc::new(Spanned::new(lowered, k.span.clone()))
                    });
                    let value = Arc::new(lower(&se.node.value));
                    core_entries.push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                }
            }
            CoreExpr::Dict(core_entries)
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // Compile-time instance dispatch rewriting: if the VarRef node for the function
            // has a call_dispatch annotation set by the type checker, rewrite the function
            // reference to the instance binding name.
            let lowered_func = if let SurfaceExpression::VarRef {
                name: _,
                resolution,
                call_dispatch,
                ..
            } = &func.expr
            {
                if let Some(binding_name) = call_dispatch.get() {
                    // Read inline resolution from the func VarRef node.
                    match resolution.get() {
                        Some(Some((level, slot))) => Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: binding_name.to_string(),
                                level,
                                slot,
                            },
                            func.span.clone(),
                        )),
                        _ => Arc::new(Spanned::new(
                            CoreExpr::Error(func.span.clone()),
                            func.span.clone(),
                        )),
                    }
                } else {
                    Arc::new(lower(func))
                }
            } else {
                Arc::new(lower(func))
            };

            CoreExpr::Call {
                func: lowered_func,
                args: args.iter().map(|a| Arc::new(lower(a))).collect(),
                named_args: named_args
                    .iter()
                    .map(|na| {
                        Spanned::new(
                            CoreNamedArg {
                                name: na.node.name.clone(),
                                value: Arc::new(lower(&na.node.value)),
                            },
                            na.span.clone(),
                        )
                    })
                    .collect(),
                implied: *implied,
            }
        }

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => CoreExpr::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    Spanned::new(
                        CoreParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: Arc::new(lower(body)),
            desugared: *desugared,
        },

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => {
            // Read the inline resolved type set by the type checker.
            // Type::Error (failed inference) → fall back to Type::Unknown (accept-all).
            // None (type checker didn't run, or --no-typecheck) → Type::Unknown.
            let ty = match resolved_type.get() {
                Some(crate::type_def::Type::Error) | None => crate::type_def::Type::Unknown,
                Some(ty) => ty.clone(),
            };
            CoreExpr::TypeAssert {
                annotation: annotation.clone(),
                expr: Arc::new(lower(inner)),
                resolved_type: ty,
                pipeline_blame: None,
            }
        }

        SurfaceExpression::Annotated { name, annotation } => CoreExpr::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },

        SurfaceExpression::Rest(name, _) => CoreExpr::Rest(name.clone()),

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower(scrutinee)),
            arms: arms
                .iter()
                .map(|arm| CoreMatchArm {
                    pattern: Spanned::new(
                        lower_pattern(&arm.pattern.node),
                        arm.pattern.span.clone(),
                    ),
                    guard: arm.guard.as_ref().map(|g| Arc::new(lower(g))),
                    body: Arc::new(lower(&arm.body)),
                })
                .collect(),
        },

        SurfaceExpression::Quote(inner) => CoreExpr::Quote(Arc::new(lower(inner))),

        SurfaceExpression::Unquote(inner) => CoreExpr::Unquote(Arc::new(lower(inner))),

        SurfaceExpression::UnquoteSplice(inner) => CoreExpr::UnquoteSplice(Arc::new(lower(inner))),

        SurfaceExpression::PatternDecl { bindings } => CoreExpr::PatternDecl {
            bindings: bindings.iter().map(|b| lower(b)).collect(),
        },

        SurfaceExpression::LetDecl { bindings } => CoreExpr::LetDecl {
            bindings: bindings
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if i % 2 == 0 {
                        lower_let_decl_binding(b)
                    } else {
                        lower(b)
                    }
                })
                .collect(),
        },

        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => CoreExpr::CaseArm {
            let_bindings: Arc::new(lower(let_bindings)),
            pattern: Arc::new(lower(pattern)),
            body: Arc::new(lower(body)),
        },

        SurfaceExpression::Placeholder => CoreExpr::Placeholder,

        SurfaceExpression::Decl(decl) => match decl.as_ref() {
            crate::ast::SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::new();
                let syn_span = rust_span!();

                for (pattern, method_entries) in arms {
                    let dispatch_tags = extract_dispatch_tags(&pattern.expr);
                    let type_args: Vec<&str> =
                        dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();

                    for me in method_entries {
                        let method_name = match me.node.key.as_ref() {
                            Some(key_node) => match &key_node.expr {
                                SurfaceExpression::Str(s) => s.clone(),
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                SurfaceExpression::Annotated { name, .. } => name.clone(),
                                _ => continue,
                            },
                            None => continue,
                        };

                        let binding_name = crate::type_def::instance_binding_name(
                            class_name,
                            &method_name,
                            &type_args,
                        );

                        let key = Some(Arc::new(Spanned::new(
                            CoreExpr::Str(binding_name),
                            syn_span.clone(),
                        )));
                        let value = Arc::new(lower(&me.node.value));
                        core_entries.push(Spanned::new(CoreEntry { key, value }, syn_span.clone()));
                    }
                }

                if !core_entries.is_empty() {
                    return CoreExpr::Dict(core_entries);
                }
                CoreExpr::Placeholder
            }
            crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                lower_type_alias_to_constructor_dict(None, body)
            }
            _ => CoreExpr::Placeholder,
        },

        SurfaceExpression::Error(span) => CoreExpr::Error(span.clone()),
    }
}

/// Convert a `Spanned<CoreExpr>` back to an `Arc<SurfaceNode>` for quote/unquote evaluation.
///
/// Bridges through `Expr` via `core_expr_to_expr` + `expr_to_surface_node`.
/// Used by the `CoreExpr::Quote` arm to get a `SurfaceNode` for `eval_quote_walk`.
///
/// The inner CoreExpr is converted back to SurfaceNode for eval_quote_walk.
/// This round-trip is necessary: Quote's inner expression is lowered (so unquote
/// expressions within it get proper variable slot resolution), but at eval time
/// the structural view is needed. CoreExpr::Var preserves the original name alongside
/// the slot, so the round-trip is lossless for variable names.
pub fn core_expr_to_surface_node(
    expr: &crate::ast::Spanned<crate::ast::CoreExpr>,
) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(
        core_expr_to_surface_expr(&expr.node),
        expr.span.clone(),
    ))
}

fn core_expr_to_surface_expr(core: &crate::ast::CoreExpr) -> SurfaceExpression {
    use crate::ast::{CoreExpr, SurfaceMatchArm};
    match core {
        CoreExpr::Int(n) => SurfaceExpression::Int(*n),
        CoreExpr::U64(n) => SurfaceExpression::U64(*n),
        CoreExpr::Float(f) => SurfaceExpression::Float(*f),
        CoreExpr::Str(s) => SurfaceExpression::Str(s.clone()),
        CoreExpr::Var { name, .. } => SurfaceExpression::VarRef {
            name: name.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
        },
        CoreExpr::Sequential(exprs) => SurfaceExpression::Sequential(
            exprs.iter().map(|e| core_expr_to_surface_node(e)).collect(),
        ),
        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
        } => SurfaceExpression::Call {
            func: core_expr_to_surface_node(func),
            args: args.iter().map(|a| core_expr_to_surface_node(a)).collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: core_expr_to_surface_node(&na.node.value),
                            annotation: None,
                        },
                        na.span.clone(),
                    )
                })
                .collect(),
            implied: *implied,
        },
        CoreExpr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => SurfaceExpression::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: core_expr_to_surface_node(body),
            desugared: *desugared,
        },
        CoreExpr::TypeAssert {
            annotation, expr, ..
        } => SurfaceExpression::TypeAssert {
            annotation: annotation.clone(),
            expr: core_expr_to_surface_node(expr),
            resolved_type: crate::ast::TypeAnnotation::new(),
        },
        CoreExpr::Annotated { name, annotation } => SurfaceExpression::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },
        CoreExpr::Rest(name) => SurfaceExpression::Rest(name.clone(), None),
        CoreExpr::Match { scrutinee, arms } => SurfaceExpression::Match {
            scrutinee: core_expr_to_surface_node(scrutinee),
            arms: arms
                .iter()
                .map(|arm| SurfaceMatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(|g| core_expr_to_surface_node(g)),
                    body: core_expr_to_surface_node(&arm.body),
                })
                .collect(),
        },
        CoreExpr::Quote(inner) => SurfaceExpression::Quote(core_expr_to_surface_node(inner)),
        CoreExpr::Unquote(inner) => SurfaceExpression::Unquote(core_expr_to_surface_node(inner)),
        CoreExpr::UnquoteSplice(inner) => {
            SurfaceExpression::UnquoteSplice(core_expr_to_surface_node(inner))
        }
        CoreExpr::PatternDecl { bindings } => SurfaceExpression::PatternDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::LetDecl { bindings } => SurfaceExpression::LetDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::Dict(entries) => SurfaceExpression::Dict(
            entries
                .iter()
                .map(|e| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceEntry {
                            key: e.node.key.as_ref().map(|k| core_expr_to_surface_node(k)),
                            value: core_expr_to_surface_node(&e.node.value),
                        },
                        e.span.clone(),
                    )
                })
                .collect(),
        ),
        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => SurfaceExpression::CaseArm {
            let_bindings: core_expr_to_surface_node(let_bindings.as_ref()),
            pattern: core_expr_to_surface_node(pattern),
            body: core_expr_to_surface_node(body),
        },
        CoreExpr::Error(span) => SurfaceExpression::Error(span.clone()),
        CoreExpr::Placeholder => SurfaceExpression::Placeholder,
        // Variant: emitted by lower.rs for type declarations; not user-writable in quotes.
        // Represent as a VarRef to the tag so quote round-trips see a name.
        CoreExpr::Variant { tag, .. } => SurfaceExpression::VarRef {
            name: tag.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
        },
    }
}

/// Lower a single binding node from a `[let ...]` declaration.
///
/// In `[let name value]` pairs (e.g., from `CoreExpr::LetDecl`), the binding name is a
/// declaration, not a variable reference. It is lowered as `CoreExpr::Str(name)` so the
/// LetDecl eval arm can extract the name directly. The value expression is lowered normally.
///
/// For annotated bindings (`name@Type`), the name is extracted and lowered as `CoreExpr::Str`.
/// For all other nodes (VarRef, Annotated, Rest), the name is extracted if possible; otherwise
/// the node is lowered normally (producing an error if unresolvable).
fn lower_let_decl_binding(arc: &Arc<SurfaceNode>) -> Spanned<CoreExpr> {
    let span = arc.span.clone();
    let core_expr = match &arc.expr {
        // Declaration name forms: lower as string literal (name extraction path)
        SurfaceExpression::VarRef { name, .. } => CoreExpr::Str(name.clone()),
        SurfaceExpression::Annotated { name, .. } => CoreExpr::Str(name.clone()),
        SurfaceExpression::Rest(Some(name), _) => CoreExpr::Str(name.clone()),
        // Wildcard / unnamed rest: use empty string (skipped by LetDecl eval arm)
        SurfaceExpression::Rest(None, _) => CoreExpr::Str(String::new()),
        // All other forms: lower normally (will produce Error if unresolvable)
        _ => lower_expr(arc, &arc.expr),
    };
    Spanned::new(core_expr, span)
}

/// Extract dispatch type tags from an instance arm pattern like `[let a@Int b@Float c]`.
///
/// Returns one `Option<String>` per binding:
/// - `Some("Int")` if the binding has a concrete uppercase type annotation
/// - `None` if unannotated or annotated with a TypeVar/complex annotation
///
/// Used by instance binding name generation in lower.rs to build the type_args for each arm.
/// Only `Some(_)` tags contribute to the binding name; trailing None entries (like the
/// return-type param `c` in Addable) are harmlessly ignored.
pub(crate) fn extract_dispatch_tags(arm_pattern: &SurfaceExpression) -> Vec<Option<String>> {
    let bindings = match arm_pattern {
        SurfaceExpression::LetDecl { bindings } => bindings,
        _ => return vec![],
    };
    bindings
        .iter()
        .map(|binding_spanned| {
            // Each binding is Annotated { name, annotation } or VarRef { name } or Str(name)
            match &binding_spanned.expr {
                SurfaceExpression::Annotated { annotation, .. } => {
                    // Extract the type name from Simple annotations with uppercase names.
                    use crate::ast::Annotation;
                    match &annotation.node {
                        Annotation::Simple(type_name)
                            if type_name
                                .chars()
                                .next()
                                .map(|c| c.is_uppercase())
                                .unwrap_or(false) =>
                        {
                            Some(type_name.clone())
                        }
                        _ => None,
                    }
                }
                _ => None, // Unannotated binding
            }
        })
        .collect()
}

/// Extract the type name from a dict entry key for TypeAlias qualified tags.
///
/// Recognized key forms (same as desugar.rs):
/// - `Str(s)` — plain string key
/// - `VarRef { name }` — bare identifier key
/// - `Annotated { name, .. }` — annotated name key (T-1052)
///
/// Returns None for computed keys or absent keys.
fn extract_type_name_from_key(key: &Option<Arc<SurfaceNode>>) -> Option<String> {
    match key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::Str(s) => Some(s.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
            _ => None, // Computed key
        },
        None => None, // Positional entry
    }
}

/// Lower a TypeAlias body to a constructor dict at runtime (T-1193).
///
/// Produces a `CoreExpr::Dict` containing constructor entries:
/// - Unit constructors (no annotation) → `CtorName: CoreExpr::Variant { tag, payload: None }`
/// - Unit constructors (with annotation) → `CtorName: [builtin-make-annotated CoreExpr::Variant { tag, payload: None } [key: val ...]]`
/// - Payload constructors → `CtorName: [fn [...fields] CoreExpr::Variant { tag, payload: Some(payload_dict) }]`
///
/// The type name (if present) qualifies the variant tags. When absent, uses unqualified tags.
///
/// Produces `CoreExpr` nodes for each constructor entry in the runtime dict.
fn lower_type_alias_to_constructor_dict(
    type_name_opt: Option<String>,
    body: &Arc<SurfaceNode>,
) -> CoreExpr {
    use crate::ast::{CoreEntry, CoreParam};

    // Extract constructors from the body using the desugar.rs helpers.
    // We need to import the extraction logic. For now, we'll inline a simplified version.
    let ctors = extract_constructors_from_body(&body.expr);

    let syn_span = rust_span!();
    let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::new();

    for ctor in ctors {
        let qualified_tag = match &type_name_opt {
            Some(tn) => format!("{}.{}", tn, ctor.name),
            None => ctor.name.clone(),
        };

        // Create the key for this constructor entry
        let key = Some(Arc::new(Spanned::new(
            CoreExpr::Str(ctor.name.clone()),
            syn_span.clone(),
        )));

        // Create the value: either a unit variant or a constructor function
        let value = if ctor.is_unit {
            // Unit constructor: CoreExpr::Variant { tag: "TypeName.CtorName", payload: None }
            // If the constructor carries a @[...] annotation (T-1121), wrap with make-annotated.
            let variant_call = Arc::new(Spanned::new(
                CoreExpr::Variant {
                    tag: qualified_tag,
                    payload: None,
                },
                syn_span.clone(),
            ));

            if let Some(ann_entries) = &ctor.annotation {
                // Build annotation dict CoreExpr from PropertyDict entries.
                // Each entry is a SurfaceEntry with a string key and literal value.
                // Lower the values through the normal lower() pipeline for correct resolution.
                let ann_core_entries: Vec<Spanned<CoreEntry>> = ann_entries
                    .iter()
                    .map(|se| {
                        let key = se.node.key.as_ref().map(|k| Arc::new(lower(k)));
                        let value = Arc::new(lower(&se.node.value));
                        Spanned::new(CoreEntry { key, value }, se.span.clone())
                    })
                    .collect();
                let ann_dict = Arc::new(Spanned::new(
                    CoreExpr::Dict(ann_core_entries),
                    syn_span.clone(),
                ));
                // [builtin-make-annotated CoreExpr::Variant{tag} [ann_entries...]]
                Arc::new(Spanned::new(
                    CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: "builtin-make-annotated".to_string(),
                                level: 0,
                                slot: 0,
                            },
                            syn_span.clone(),
                        )),
                        args: vec![variant_call, ann_dict],
                        named_args: vec![],
                        implied: false,
                    },
                    syn_span.clone(),
                ))
            } else {
                variant_call
            }
        } else {
            // Payload constructor: function that takes named args and returns a variant
            // [fn [let ...fields] CoreExpr::Variant{tag, payload: Some(payload_dict)}]
            let params: Vec<Spanned<CoreParam>> = ctor
                .fields
                .iter()
                .map(|field_name| {
                    Spanned::new(
                        CoreParam {
                            name: field_name.clone(),
                            annotation: None,
                            variadic: false,
                        },
                        syn_span.clone(),
                    )
                })
                .collect();

            // Build the payload dict: [dict field: field-value ...]
            //
            // CRITICAL: Use CoreExpr::Var { level: 1, slot: idx } here. The payload dict
            // is evaluated by eval_dict_core which creates a letrec environment where each
            // field name is bound as a key. Using Var { level: 0 } would resolve in the dict's
            // own letrec env, finding the dict's own "field" entry (the thunk being forced),
            // and triggering E070 circular dependency.
            //
            // Var { level: 1 } skips one level up past the dict's letrec env to the
            // function's call env (created by bind_args_thunks), where the param is
            // bound at slot `idx`. This correctly references the caller's argument
            // without shadowing through the payload dict's letrec scope.
            let payload_entries: Vec<Spanned<CoreEntry>> = ctor
                .fields
                .iter()
                .enumerate()
                .map(|(idx, field_name)| {
                    Spanned::new(
                        CoreEntry {
                            key: Some(Arc::new(Spanned::new(
                                CoreExpr::Str(field_name.clone()),
                                syn_span.clone(),
                            ))),
                            value: Arc::new(Spanned::new(
                                // level=1: one env level up from the payload dict's letrec
                                // env → reaches the function's call env (bind_args_thunks).
                                // slot=idx: params are inserted in declaration order, so
                                // the i-th field is at slot i in the call env.
                                CoreExpr::Var {
                                    name: field_name.clone(),
                                    level: 1,
                                    slot: idx as u32,
                                },
                                syn_span.clone(),
                            )),
                        },
                        syn_span.clone(),
                    )
                })
                .collect();

            let payload_dict = CoreExpr::Dict(payload_entries);

            // Build CoreExpr::Variant { tag: "TypeName.CtorName", payload: Some(payload_dict) }
            let variant_call = CoreExpr::Variant {
                tag: qualified_tag,
                payload: Some(Arc::new(Spanned::new(payload_dict, syn_span.clone()))),
            };

            let fn_expr = Arc::new(Spanned::new(
                CoreExpr::Fn {
                    return_ann: None,
                    params,
                    body: Arc::new(Spanned::new(variant_call, syn_span.clone())),
                    desugared: false,
                },
                syn_span.clone(),
            ));

            // Wrap payload constructor with annotation if present (mirrors unit constructor handling)
            if let Some(ann_entries) = &ctor.annotation {
                let ann_core_entries: Vec<Spanned<CoreEntry>> = ann_entries
                    .iter()
                    .map(|se| {
                        let key = se.node.key.as_ref().map(|k| Arc::new(lower(k)));
                        let value = Arc::new(lower(&se.node.value));
                        Spanned::new(CoreEntry { key, value }, se.span.clone())
                    })
                    .collect();
                let ann_dict = Arc::new(Spanned::new(
                    CoreExpr::Dict(ann_core_entries),
                    syn_span.clone(),
                ));
                Arc::new(Spanned::new(
                    CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: "builtin-make-annotated".to_string(),
                                level: 0,
                                slot: 0,
                            },
                            syn_span.clone(),
                        )),
                        args: vec![fn_expr, ann_dict],
                        named_args: vec![],
                        implied: false,
                    },
                    syn_span.clone(),
                ))
            } else {
                fn_expr
            }
        };

        core_entries.push(Spanned::new(CoreEntry { key, value }, syn_span.clone()));
    }

    CoreExpr::Dict(core_entries)
}

/// Simplified constructor info for lowering.
struct ConstructorInfo {
    name: String,
    is_unit: bool,
    fields: Vec<String>,
    /// Annotation entries from `@[...]` on the constructor declaration (T-1121).
    /// Present when the constructor was written as `CtorName@[key: val ...]` in the type body.
    /// Used by `lower_type_alias_to_constructor_dict` to emit `[builtin-make-annotated ...]`.
    annotation: Option<Vec<Spanned<SurfaceEntry>>>,
}

/// Extract constructor information from a TypeAlias body.
///
/// Handles the common constructor forms:
/// 1. Bare VarRef uppercase → unit constructor (e.g., `Red`, `None`)
/// 2. Annotated uppercase → unit constructor with annotation
/// 3. Call with uppercase func + no named args → unit constructor (e.g., `[Ok a]`, `[Error String]`)
/// 4. Call with uppercase func + named args → named-field constructor (e.g., `[Circle r: Int]`)
/// 5. Dict with first positional VarRef/Annotated + keyed entries → named-field constructor
fn extract_constructors_from_body(body: &SurfaceExpression) -> Vec<ConstructorInfo> {
    let mut ctors = Vec::new();

    fn is_ctor(s: &str) -> bool {
        crate::eval::is_constructor_name(s)
    }

    fn try_extract_one(expr: &SurfaceExpression, ctors: &mut Vec<ConstructorInfo>) {
        match expr {
            // Bare uppercase VarRef → unit constructor (no annotation)
            SurfaceExpression::VarRef { name, .. } if is_ctor(name) => {
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    fields: Vec::new(),
                    annotation: None,
                });
            }
            // Annotated uppercase → unit constructor with annotation (T-1121).
            // `Red@[category: "primary"]` → unit constructor carrying the PropertyDict entries.
            SurfaceExpression::Annotated { name, annotation } if is_ctor(name) => {
                // Extract PropertyDict entries for annotation wrapping at runtime.
                let ann_entries = match &annotation.node {
                    crate::ast::Annotation::PropertyDict(entries) if !entries.is_empty() => {
                        Some(entries.clone())
                    }
                    _ => None,
                };
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    fields: Vec::new(),
                    annotation: ann_entries,
                });
            }
            // Call with uppercase func → unit or named-field constructor
            // [Ok a] → Call { func: VarRef("Ok"), args: [VarRef("a")], named_args: [] } → unit
            // [Circle r: Int] → Call { func: VarRef("Circle"), named_args: [(r, Int)] } → named-field
            //
            // T-1357: With lookup-table constants, `named_args` may contain a mix of:
            //   - Constant entries: `name: literal` (Int/Float/Str/U64 value) → NOT a runtime field
            //   - Payload field entries: `name: TypeExpr` (non-literal) → runtime field
            // And `args` may contain:
            //   - Annotated positional entries: `name@TypeExpr` → named runtime payload field
            //   - Bare positional entries: old-style positional payload (type params for unit ctors)
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if is_ctor(name) {
                        // Payload fields from named_args: only non-literal values are runtime fields.
                        // Literal values (Int/Float/Str/U64) are compile-time constants.
                        let is_literal = |expr: &SurfaceExpression| {
                            matches!(
                                expr,
                                SurfaceExpression::Int(_)
                                    | SurfaceExpression::U64(_)
                                    | SurfaceExpression::Float(_)
                                    | SurfaceExpression::Str(_)
                            )
                        };
                        let payload_named_fields: Vec<String> = named_args
                            .iter()
                            .filter(|na| !is_literal(&na.node.value.expr))
                            .map(|na| na.node.name.clone())
                            .collect();

                        // Payload fields from annotated positional args (data@String form).
                        let payload_annotated_fields: Vec<String> = args
                            .iter()
                            .filter_map(|arg| {
                                if let SurfaceExpression::Annotated { name, .. } = &arg.expr {
                                    Some(name.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        let fields: Vec<String> = payload_named_fields
                            .into_iter()
                            .chain(payload_annotated_fields)
                            .collect();

                        let is_unit = fields.is_empty();
                        ctors.push(ConstructorInfo {
                            name: name.clone(),
                            is_unit,
                            fields,
                            annotation: None,
                        });
                    }
                }
            }
            // Dict `[Constructor field: Type ...]` — single named-field constructor
            SurfaceExpression::Dict(entries) if !entries.is_empty() => {
                let first = &entries[0];
                if first.node.key.is_some() {
                    return;
                }
                // Extract constructor name and annotation from the first (positional) entry
                let (ctor_name, ctor_annotation) = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } if is_ctor(name) => (name.clone(), None),
                    SurfaceExpression::Annotated { name, annotation } if is_ctor(name) => {
                        // Extract PropertyDict annotation entries for the constructor
                        let ann = match &annotation.node {
                            crate::ast::Annotation::PropertyDict(entries)
                                if !entries.is_empty() =>
                            {
                                Some(entries.clone())
                            }
                            _ => None,
                        };
                        (name.clone(), ann)
                    }
                    _ => return,
                };
                let fields: Vec<String> = entries[1..]
                    .iter()
                    .filter_map(|e| {
                        let key = e.node.key.as_ref()?;
                        match &key.expr {
                            SurfaceExpression::Str(s) => Some(s.clone()),
                            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                            _ => None,
                        }
                    })
                    .collect();
                let is_unit = fields.is_empty();
                ctors.push(ConstructorInfo {
                    name: ctor_name,
                    is_unit,
                    fields,
                    annotation: ctor_annotation,
                });
            }
            _ => {}
        }
    }

    // Top-level dispatch: distinguish single-constructor dict from union of constructors.
    match body {
        SurfaceExpression::Dict(entries) => {
            // Distinguish "single named-field constructor dict" from "union of constructors":
            // - Constructor dict: first positional is VarRef/Annotated uppercase AND has keyed entries
            // - Union: each positional entry is a separate constructor
            let is_single_ctor_dict = entries.first().is_some_and(|first| {
                if first.node.key.is_some() {
                    return false;
                }
                let first_is_ctor = matches!(&first.node.value.expr,
                    SurfaceExpression::VarRef { name, .. } if is_ctor(name))
                    || matches!(&first.node.value.expr,
                    SurfaceExpression::Annotated { name, .. } if is_ctor(name));
                let has_keyed = entries[1..].iter().any(|e| e.node.key.is_some());
                first_is_ctor && has_keyed
            });
            if is_single_ctor_dict {
                try_extract_one(body, &mut ctors);
            } else {
                for entry in entries {
                    if entry.node.key.is_none() {
                        try_extract_one(&entry.node.value.expr, &mut ctors);
                    }
                }
            }
        }
        other => try_extract_one(other, &mut ctors),
    }
    ctors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        CallDispatch, Provenance, Resolution, SurfaceExpression, SurfaceNode, TypeAnnotation,
    };
    use std::sync::Arc;

    fn make_node(expr: SurfaceExpression, span: crate::ast::Span) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span,
            type_guard: TypeAnnotation::new(),
            provenance: Provenance::new(),
        })
    }

    #[test]
    fn test_lower_int_literal() {
        let span = rust_span!();
        let node = make_node(SurfaceExpression::Int(42), span.clone());

        let lowered = lower(&node);

        assert_eq!(lowered.span, span);
        assert!(matches!(lowered.node, CoreExpr::Int(42)));
    }

    #[test]
    fn test_lower_varref_with_resolution() {
        let span = rust_span!();
        // Build a VarRef node with pre-set inline resolution (level=0, slot=3).
        let resolution = Resolution::new();
        resolution.set(Some((0, 3)));
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "x".into(),
                escaped: false,
                resolution,
                call_dispatch: CallDispatch::new(),
            },
            span,
        );

        let lowered = lower(&node);

        match lowered.node {
            CoreExpr::Var { name, level, slot } => {
                assert_eq!(name, "x");
                assert_eq!(level, 0);
                assert_eq!(slot, 3);
            }
            _ => panic!("expected CoreExpr::Var, got {:?}", lowered.node),
        }
    }

    #[test]
    fn test_lower_varref_without_resolution() {
        let span = rust_span!();
        // VarRef with no resolution set (resolution field left at default = not yet resolved).
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "unbound".into(),
                escaped: false,
                resolution: Resolution::new(), // Not set — resolver never ran
                call_dispatch: CallDispatch::new(),
            },
            span.clone(),
        );

        let lowered = lower(&node);

        // Unresolvable VarRef produces CoreExpr::Error — a genuine compile error.
        assert!(
            matches!(lowered.node, CoreExpr::Error(_)),
            "expected CoreExpr::Error for unresolvable VarRef, got {:?}",
            lowered.node
        );
    }
}
