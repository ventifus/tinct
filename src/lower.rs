//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `SurfaceNode` — all cross-phase data lives inline on nodes.
//! De Bruijn coordinates are read from the inline `resolution` field on VarRef/Field nodes.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `Error` (unresolvable — genuine compile error)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (with resolved_type from the inline TypeAnnotation field or Type::Unknown)
//! - `TypeAssertPending` in patterns → `TypeAssert` (using the inline `resolved` TypeAnnotation field)
//! - `Field` with `field_slot` set → `Call(slot-get, [Int(slot), target])` (O(1) positional access)
//! - `Field` without `field_slot` → `Call(field-get, [Str/Int(key), target])` (key-based lookup)
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
        Pattern::Predicate { .. } => pat.clone(),
    }
}

fn lower_expr(arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int(*n),
        SurfaceExpression::U64(n) => CoreExpr::U64(*n),
        SurfaceExpression::Float(n) => CoreExpr::Float(*n),
        SurfaceExpression::Str(s) => CoreExpr::Str(s.clone()),

        SurfaceExpression::VarRef {
            name, resolution, annotation, ..
        } => {
            match resolution.get() {
                Some(Some((level, slot))) => CoreExpr::Var {
                    name: name.clone(),
                    level,
                    slot,
                    annotation: annotation.clone(),
                },
                _ => {
                    // Some(None) = unresolvable; None = resolver never ran.
                    // Both are genuine compile errors — produce Error so the evaluator
                    // surfaces it at force time.
                    CoreExpr::Error(arc.span.clone())
                }
            }
        }

        SurfaceExpression::Field {
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
                    annotation: None,
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
        SurfaceExpression::Field {
            expr: None,
            field: crate::ast::DotKey::Ident(name),
            resolution,
            ..
        } => match resolution.get() {
            Some(Some((level, slot))) => CoreExpr::Var {
                name: name.clone(),
                level,
                slot,
                annotation: None,
            },
            _ => CoreExpr::Error(arc.span.clone()),
        },

        // Leading-dot with integer key: `.0` — no parent-scope numeric lookup. The parser
        // rejects this at parse time, so this is a safety fallback only.
        SurfaceExpression::Field {
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
                        crate::ast::SurfaceDeclaration::InstanceDecl {
                            class_name,
                            arms,
                        } => {
                            if se.node.key.is_some() {
                                // Named instance: emit outer key binding only.
                                // Binding names are NOT flattened to avoid duplicate key errors
                                // when multiple instances of the same class exist in the dict.
                                let lowered = lower_expr(&se.node.value, &se.node.value.expr);
                                let key = se.node.key.as_ref().map(|k| Arc::new(lower(k)));
                                let value = Arc::new(Spanned::new(lowered, se.span.clone()));
                                core_entries
                                    .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                            } else {
                                // Anonymous instance: flatten binding names into the outer dict.
                                // Matches the synthetic slots surface_dict_static_keys injects.
                                for (pattern, method_entries) in arms {
                                    let dispatch_tags = extract_dispatch_tags(&pattern.expr);
                                    let type_args: Vec<&str> = dispatch_tags
                                        .iter()
                                        .filter_map(|t| t.as_deref())
                                        .collect();
                                    for me in method_entries {
                                        let method_name = match me.node.key.as_ref() {
                                            Some(key_node) => match &key_node.expr {
                                                SurfaceExpression::Str(s) => s.clone(),
                                                // Both plain and annotated VarRef use the name field.
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    name.clone()
                                                }
                                                _ => continue,
                                            },
                                            None => continue,
                                        };
                                        let binding_name =
                                            crate::type_def::instance_binding_name(
                                                class_name,
                                                &method_name,
                                                &type_args,
                                            );
                                        let key = Some(Arc::new(Spanned::new(
                                            CoreExpr::Str(binding_name),
                                            se.span.clone(),
                                        )));
                                        let value = Arc::new(lower(&me.node.value));
                                        core_entries.push(Spanned::new(
                                            CoreEntry { key, value },
                                            se.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                            let type_name_opt = extract_type_name_from_key(&se.node.key);
                            let ctor_dict =
                                lower_type_alias_to_constructor_dict(type_name_opt, body);
                            let key = se.node.key.as_ref().map(|k| {
                                let lowered = match &k.expr {
                                    // Both plain and annotated VarRef use the name field.
                                    SurfaceExpression::VarRef { name, .. } => {
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
                        crate::ast::SurfaceDeclaration::ClassDecl { .. } => {
                            // Named ClassDecl: emit an empty-dict runtime value so the outer
                            // key occupies a slot. This allows leading-dot re-exports like
                            // `Indexable: .Indexable` to reference the class across dict
                            // boundaries. Class methods are not emitted here.
                            if se.node.key.is_some() {
                                let key = se.node.key.as_ref().map(|k| {
                                    let lowered = match &k.expr {
                                        // Both plain and annotated VarRef use the name field.
                                        SurfaceExpression::VarRef { name, .. } => {
                                            CoreExpr::Str(name.clone())
                                        }
                                        _ => lower_expr(k, &k.expr),
                                    };
                                    Arc::new(Spanned::new(lowered, k.span.clone()))
                                });
                                let value = Arc::new(Spanned::new(
                                    CoreExpr::Dict(vec![]),
                                    se.span.clone(),
                                ));
                                core_entries
                                    .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                            }
                        }
                        _ => {
                            continue;
                        }
                    }
                } else {
                    let key = se.node.key.as_ref().map(|k| {
                        let lowered = match &k.expr {
                            // Both plain and annotated VarRef use the name field.
                            SurfaceExpression::VarRef { name, .. } => CoreExpr::Str(name.clone()),
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
                name,
                resolution: _,
                call_dispatch,
                ..
            } = &func.expr
            {
                if let Some((level, slot)) = call_dispatch.get() {
                    // Direct de Bruijn lookup — coordinates set by the type checker.
                    // No name-based fallback; if coords are present they are authoritative.
                    Arc::new(Spanned::new(
                        CoreExpr::Var {
                            name: name.clone(),
                            level,
                            slot,
                            annotation: None,
                        },
                        func.span.clone(),
                    ))
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
                Some(crate::type_def::Type::Error(_)) | None => crate::type_def::Type::Unknown,
                Some(ty) => ty.clone(),
            };
            CoreExpr::TypeAssert {
                annotation: annotation.clone(),
                expr: Arc::new(lower(inner)),
                resolved_type: ty,
                pipeline_blame: None,
            }
        }

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
                                // Both plain and annotated VarRef use the name field.
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
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
            crate::ast::SurfaceDeclaration::TypeAlias { .. } => {
                // Type declarations in standalone expression position produce no runtime value
                // (B-430). The dict-entry case (lower.rs Dict arm, line ~309) calls
                // lower_type_alias_to_constructor_dict to produce constructor entries under the
                // declared name. Here (direct Decl, no enclosing dict entry), the declaration is
                // not bound to any name so there are no constructor entries to emit — return {}.
                CoreExpr::Dict(vec![])
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
        CoreExpr::Var { name, annotation, .. } => SurfaceExpression::VarRef {
            name: name.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: annotation.clone(),
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
            annotation: None,
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
        // Annotated VarRef (name@Type) is also lowered to Str — the annotation is stripped.
        SurfaceExpression::VarRef { name, .. } => CoreExpr::Str(name.clone()),
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
            // Each binding is VarRef { annotation: Some(_) } or VarRef { annotation: None } or Str(name)
            match &binding_spanned.expr {
                SurfaceExpression::VarRef { annotation: Some(ann), .. } => {
                    // Extract the type name from annotations. VarRef annotations are normalized
                    // from Simple("T") to PropertyDict{type: VarRef("T")} at parse time, so we
                    // handle both forms. Only uppercase type names are valid dispatch tags.
                    use crate::ast::Annotation;
                    let type_name_opt = match &ann.node {
                        Annotation::Simple(type_name) => Some(type_name.as_str()),
                        Annotation::PropertyDict(entries) => {
                            // Find the "type" key entry and extract its VarRef name.
                            entries.iter().find_map(|e| {
                                let key_str = e.node.key.as_ref().and_then(|k| {
                                    if let SurfaceExpression::Str(s) = &k.expr {
                                        Some(s.as_str())
                                    } else {
                                        None
                                    }
                                });
                                if key_str == Some("type") {
                                    if let SurfaceExpression::VarRef { name, .. } = &e.node.value.expr {
                                        Some(name.as_str())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                        }
                        _ => None,
                    };
                    type_name_opt
                        .filter(|n| n.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
                        .map(|n| n.to_string())
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
            // Both plain VarRef and annotated VarRef (name@Type) use the name field directly.
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
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
    use crate::ast::CoreEntry;

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
                                annotation: None,
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
            // Named-field payload constructor: emit a function that accepts the fields
            // as named parameters and constructs a Variant with the payload dict.
            //
            // The function carries `return_ann: Some(Annotation::Simple(qualified_tag))`
            // so that pattern matching can identify the constructor tag via the function's
            // return annotation, without any special "constructor" runtime type.
            //
            // Example: `[type ProgramItem [File path: String handle: Handle]]` produces:
            //   `File: [fn@"ProgramItem.File" [let path handle]
            //             [Variant "ProgramItem.File" {path: $path, handle: $handle}]]`
            let fields = &ctor.fields;

            // Build one CoreParam per field.
            let fn_params: Vec<Spanned<crate::ast::CoreParam>> = fields
                .iter()
                .map(|field_name| {
                    Spanned::new(
                        crate::ast::CoreParam {
                            name: field_name.clone(),
                            annotation: None,
                            variadic: false,
                        },
                        syn_span.clone(),
                    )
                })
                .collect();

            // Build the payload dict: {field0: $field0, field1: $field1, ...}
            // Each entry is a string-keyed CoreEntry pointing to the corresponding param.
            // level=1 skips the function body's own letrec env to reach the params.
            let payload_entries: Vec<Spanned<CoreEntry>> = fields
                .iter()
                .enumerate()
                .map(|(idx, field_name)| {
                    let key = Some(Arc::new(Spanned::new(
                        CoreExpr::Str(field_name.clone()),
                        syn_span.clone(),
                    )));
                    let value = Arc::new(Spanned::new(
                        CoreExpr::Var {
                            name: field_name.clone(),
                            level: 1,
                            slot: idx as u32,
                            annotation: None,
                        },
                        syn_span.clone(),
                    ));
                    Spanned::new(CoreEntry { key, value }, syn_span.clone())
                })
                .collect();

            let payload_dict = Arc::new(Spanned::new(
                CoreExpr::Dict(payload_entries),
                syn_span.clone(),
            ));

            // Build the variant body: Variant { tag, payload: Some(payload_dict) }
            let variant_body = Arc::new(Spanned::new(
                CoreExpr::Variant {
                    tag: qualified_tag.clone(),
                    payload: Some(payload_dict),
                },
                syn_span.clone(),
            ));

            // Build the return annotation — Annotation::Simple(qualified_tag) so pattern
            // matching can extract the tag from the function's return_ann field.
            let fn_return_ann = Some(Spanned::new(
                crate::ast::Annotation::Simple(qualified_tag.clone()),
                syn_span.clone(),
            ));

            let fn_expr = Arc::new(Spanned::new(
                CoreExpr::Fn {
                    return_ann: fn_return_ann,
                    params: fn_params,
                    body: variant_body,
                    desugared: false,
                },
                syn_span.clone(),
            ));

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
                                annotation: None,
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
    /// Annotation entries from `@[...]` on the constructor declaration.
    annotation: Option<Vec<Spanned<SurfaceEntry>>>,
    /// Field names for named-field constructors (empty for unit constructors).
    fields: Vec<String>,
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
            // Uppercase VarRef → unit constructor.
            // May carry a PropertyDict annotation (`Red@[category: "primary"]`).
            SurfaceExpression::VarRef { name, annotation, .. } if is_ctor(name) => {
                let ann_entries = annotation.as_ref().and_then(|ann| {
                    match &ann.node {
                        crate::ast::Annotation::PropertyDict(entries) if !entries.is_empty() => {
                            Some(entries.clone())
                        }
                        _ => None,
                    }
                });
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    annotation: ann_entries,
                    fields: vec![],
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
                        // Now represented as VarRef { name, annotation: Some(_) }.
                        let payload_annotated_fields: Vec<String> = args
                            .iter()
                            .filter_map(|arg| {
                                if let SurfaceExpression::VarRef { name, annotation: Some(_), .. } = &arg.expr {
                                    Some(name.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        let is_unit = payload_named_fields.is_empty()
                            && payload_annotated_fields.is_empty();
                        let fields = if is_unit {
                            vec![]
                        } else {
                            // Collect all payload fields: annotated positional args first,
                            // then named args (non-literal values are runtime fields).
                            let mut all_fields = payload_annotated_fields;
                            all_fields.extend(payload_named_fields);
                            all_fields
                        };
                        ctors.push(ConstructorInfo {
                            name: name.clone(),
                            is_unit,
                            annotation: None,
                            fields,
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
                // Extract constructor name and annotation from the first (positional) entry.
                // Both plain and annotated VarRef are now VarRef { name, annotation }.
                let (ctor_name, ctor_annotation) = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, annotation, .. } if is_ctor(name) => {
                        let ann = annotation.as_ref().and_then(|ann| {
                            match &ann.node {
                                crate::ast::Annotation::PropertyDict(entries)
                                    if !entries.is_empty() =>
                                {
                                    Some(entries.clone())
                                }
                                _ => None,
                            }
                        });
                        (name.clone(), ann)
                    }
                    _ => return,
                };
                let is_unit = entries[1..].is_empty()
                    || entries[1..].iter().all(|e| e.node.key.is_none());
                // Collect field names from keyed entries for named-field constructors.
                let fields: Vec<String> = if is_unit {
                    vec![]
                } else {
                    entries[1..]
                        .iter()
                        .filter_map(|e| {
                            e.node.key.as_ref().and_then(|k| match &k.expr {
                                SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                                SurfaceExpression::Str(s) => Some(s.clone()),
                                _ => None,
                            })
                        })
                        .collect()
                };
                ctors.push(ConstructorInfo {
                    name: ctor_name,
                    is_unit,
                    annotation: ctor_annotation,
                    fields,
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
                // Both plain and annotated VarRef are now VarRef { name, annotation }.
                let first_is_ctor = matches!(&first.node.value.expr,
                    SurfaceExpression::VarRef { name, .. } if is_ctor(name));
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
        CallDispatch, Provenance, Resolution, SurfaceDeclaration, SurfaceExpression, SurfaceNode,
        TypeAnnotation,
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
                annotation: None,
            },
            span,
        );

        let lowered = lower(&node);

        match lowered.node {
            CoreExpr::Var { name, level, slot, .. } => {
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
                annotation: None,
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

    // B-430: [type MyType Int] in standalone expression position must lower to an empty dict.
    //
    // Type declarations produce no runtime value when they appear as standalone expressions
    // (not as dict-entry values). The correct runtime representation is {} (empty dict).
    // Previously this called lower_type_alias_to_constructor_dict(None, body), which would
    // misinterpret "Int" (an uppercase VarRef) as a unit constructor and produce a non-empty
    // dict with a spurious "Int" entry.
    #[test]
    fn test_lower_type_alias_standalone_returns_empty_dict() {
        let span = rust_span!();
        // [type MyType Int] — TypeAlias with body = VarRef("Int"), no params.
        let body = make_node(
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
                resolution: Resolution::new(),
                call_dispatch: CallDispatch::new(),
                annotation: None,
            },
            span.clone(),
        );
        let node = make_node(
            SurfaceExpression::Decl(Box::new(SurfaceDeclaration::TypeAlias {
                params: vec![],
                body,
            })),
            span,
        );

        let lowered = lower(&node);

        match lowered.node {
            CoreExpr::Dict(entries) => assert!(
                entries.is_empty(),
                "B-430: standalone [type ...] must lower to empty dict, got {} entries",
                entries.len()
            ),
            other => panic!(
                "B-430: expected CoreExpr::Dict([]) for standalone TypeAlias, got {:?}",
                other
            ),
        }
    }

    // B-430 variant: [type Color Red Green Blue] standalone also lowers to empty dict.
    //
    // Even with legitimate constructors (Red, Green, Blue), a TypeAlias in standalone
    // expression position (no enclosing dict entry with a name) should return {}.
    // The constructors are only accessible when the TypeAlias is bound to a name in a dict
    // entry (e.g. `Color: [type Red Green Blue]`), which is handled by the Dict lowering arm.
    #[test]
    fn test_lower_type_alias_standalone_sum_type_returns_empty_dict() {
        let span = rust_span!();
        // Body: dict with positional entries [Red Green Blue]
        let make_ctor = |name: &str| {
            Spanned::new(
                crate::ast::SurfaceEntry {
                    key: None,
                    value: make_node(
                        SurfaceExpression::VarRef {
                            name: name.into(),
                            escaped: false,
                            resolution: Resolution::new(),
                            call_dispatch: CallDispatch::new(),
                            annotation: None,
                        },
                        span.clone(),
                    ),
                },
                span.clone(),
            )
        };
        let body = make_node(
            SurfaceExpression::Dict(vec![
                make_ctor("Red"),
                make_ctor("Green"),
                make_ctor("Blue"),
            ]),
            span.clone(),
        );
        let node = make_node(
            SurfaceExpression::Decl(Box::new(SurfaceDeclaration::TypeAlias {
                params: vec![],
                body,
            })),
            span,
        );

        let lowered = lower(&node);

        match lowered.node {
            CoreExpr::Dict(entries) => assert!(
                entries.is_empty(),
                "B-430: standalone [type Red Green Blue] must lower to empty dict, got {} entries",
                entries.len()
            ),
            other => panic!(
                "B-430: expected CoreExpr::Dict([]) for standalone sum-type TypeAlias, got {:?}",
                other
            ),
        }
    }
}
