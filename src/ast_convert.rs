//! Conversion from the old Expr/Document/File AST to the Surface AST types.
// Transitional bridge — deleted in Sprint 1 Part E.
#![allow(dead_code)]
//!
//! This module provides a bridge so the Surface types can be used immediately
//! without migrating the 8000-line parser in one go. The parser continues to
//! produce File/Document/Expr; callers that need SurfaceProgram call
//! `file_to_surface_program()`.
//!
//! This is a transitional module deleted in Sprint 1, Part E when the parser
//! is migrated to produce SurfaceProgram directly.

use std::sync::Arc;

use crate::ast::{
    node_id, Document, Entry, Expr, File, MatchArm, Spanned, SurfaceDeclaration, SurfaceDocument,
    SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode,
    SurfaceParam, SurfaceProgram, TypeAnnotationTable,
};

/// Convert a parsed `File` to a `SurfaceProgram`.
///
/// This is the bridge between the old parser output and the Surface AST.
/// Called by `parse_surface()` and any code that needs `SurfaceProgram`.
///
/// Declaration forms (TypeAlias, ClassDecl, InstanceDecl, DefMacro, MacroDecl,
/// SyntaxClass, Splice) become `SurfaceItem::Decl`; everything else becomes
/// `SurfaceItem::Expr`.
pub fn file_to_surface_program(file: &File) -> SurfaceProgram {
    SurfaceProgram {
        documents: file
            .documents
            .iter()
            .map(|doc_spanned| {
                Spanned::new(document_to_surface(&doc_spanned.node), doc_spanned.span)
            })
            .collect(),
    }
}

/// Convert a typechecked `File` to a `SurfaceProgram` AND extract `TypeAnnotationTable`.
///
/// Requires that `typecheck_file()` has already been called on the file so that
/// `TypeAssert.resolved_type` RefCells are populated. During the bridge conversion,
/// each `TypeAssert` node in the old File that has a resolved type is recorded in
/// the table keyed by the corresponding `SurfaceNode`'s `NodeId`.
///
/// Deleted in Part E when the typechecker directly produces `TypeAnnotationTable`.
pub fn file_to_surface_program_with_types(file: &File) -> (SurfaceProgram, TypeAnnotationTable) {
    let mut table = TypeAnnotationTable::new();
    let program = file_to_surface_program_collecting(file, &mut table);
    (program, table)
}

fn file_to_surface_program_collecting(
    file: &File,
    table: &mut TypeAnnotationTable,
) -> SurfaceProgram {
    SurfaceProgram {
        documents: file
            .documents
            .iter()
            .map(|doc_spanned| {
                Spanned::new(
                    document_to_surface_collecting(&doc_spanned.node, table),
                    doc_spanned.span,
                )
            })
            .collect(),
    }
}

fn document_to_surface_collecting(
    doc: &Document,
    table: &mut TypeAnnotationTable,
) -> SurfaceDocument {
    let items = doc
        .expressions
        .iter()
        .map(|expr_rc| expr_to_surface_item_collecting(expr_rc, table))
        .collect();
    SurfaceDocument {
        stage: doc.stage.clone(),
        name: doc.name.clone(),
        items,
        output_type: doc.output_type.clone(),
        expects: doc.expects.clone(),
        caps: doc.caps.clone(),
    }
}

fn expr_to_surface_item_collecting(
    spanned: &Spanned<Expr>,
    table: &mut TypeAnnotationTable,
) -> SurfaceItem {
    // First convert to item
    let item = expr_to_surface_item(spanned);
    // Then extract TypeAnnotationTable entries from TypeAssert nodes
    if let SurfaceItem::Expr(ref node) = item {
        collect_type_annotations_from_expr(spanned, node, table);
    }
    item
}

fn collect_type_annotations_from_expr(
    old_expr: &Spanned<Expr>,
    new_node: &Arc<SurfaceNode>,
    table: &mut TypeAnnotationTable,
) {
    match &old_expr.node {
        Expr::TypeAssert {
            resolved_type,
            expr: inner,
            ..
        } => {
            // Extract resolved type from RefCell if typechecking has populated it
            if let Some(ty) = resolved_type.borrow().as_ref().cloned() {
                table.insert(node_id(new_node), ty);
            }
            // Recurse into inner — find the corresponding SurfaceNode
            if let SurfaceExpression::TypeAssert {
                expr: inner_surface,
                ..
            } = &new_node.expr
            {
                collect_type_annotations_from_expr(inner, inner_surface, table);
            }
        }
        // Recurse into children for all other expressions
        Expr::Dict(entries) => {
            if let SurfaceExpression::Dict(surface_entries) = &new_node.expr {
                for (old_e, new_e) in entries.iter().zip(surface_entries.iter()) {
                    if let Some(ref old_key) = old_e.node.key {
                        if let Some(ref new_key) = new_e.node.key {
                            collect_type_annotations_from_expr(old_key, new_key, table);
                        }
                    }
                    collect_type_annotations_from_expr(&old_e.node.value, &new_e.node.value, table);
                }
            }
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            if let SurfaceExpression::Call {
                func: sf,
                args: sa,
                named_args: sna,
                ..
            } = &new_node.expr
            {
                collect_type_annotations_from_expr(func, sf, table);
                for (old_a, new_a) in args.iter().zip(sa.iter()) {
                    collect_type_annotations_from_expr(old_a, new_a, table);
                }
                for (old_na, new_na) in named_args.iter().zip(sna.iter()) {
                    collect_type_annotations_from_expr(
                        &old_na.node.value,
                        &new_na.node.value,
                        table,
                    );
                }
            }
        }
        Expr::Fn { body, .. } => {
            if let SurfaceExpression::Fn { body: sb, .. } = &new_node.expr {
                collect_type_annotations_from_expr(body, sb, table);
            }
        }
        Expr::Sequential(exprs) => {
            if let SurfaceExpression::Sequential(surface_exprs) = &new_node.expr {
                for (old_e, new_e) in exprs.iter().zip(surface_exprs.iter()) {
                    collect_type_annotations_from_expr(old_e, new_e, table);
                }
            }
        }
        Expr::DotAccess { expr, .. } => {
            if let SurfaceExpression::DotAccess { expr: se, .. } = &new_node.expr {
                collect_type_annotations_from_expr(expr, se, table);
            }
        }
        Expr::Match { scrutinee, arms } => {
            if let SurfaceExpression::Match {
                scrutinee: ss,
                arms: sa,
            } = &new_node.expr
            {
                collect_type_annotations_from_expr(scrutinee, ss, table);
                for (old_arm, new_arm) in arms.iter().zip(sa.iter()) {
                    if let Some(ref old_g) = old_arm.guard {
                        if let Some(ref new_g) = new_arm.guard {
                            collect_type_annotations_from_expr(old_g, new_g, table);
                        }
                    }
                    collect_type_annotations_from_expr(&old_arm.body, &new_arm.body, table);
                }
            }
        }
        _ => {} // Literals and non-recursive forms have no TypeAssert children
    }
}

fn document_to_surface(doc: &Document) -> SurfaceDocument {
    let items = doc
        .expressions
        .iter()
        .map(|expr_rc| expr_to_surface_item(expr_rc))
        .collect();

    SurfaceDocument {
        stage: doc.stage.clone(),
        name: doc.name.clone(),
        items,
        output_type: doc.output_type.clone(),
        expects: doc.expects.clone(),
        caps: doc.caps.clone(),
    }
}

fn expr_to_surface_item(spanned: &Spanned<Expr>) -> SurfaceItem {
    match &spanned.node {
        // Compile-time-only declaration forms → SurfaceItem::Decl
        Expr::TypeAlias { params, body } => {
            let decl = SurfaceDeclaration::TypeAlias {
                params: params.clone(),
                body: expr_to_surface_node(body),
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::ClassDecl {
            name,
            params,
            superclasses,
            methods,
            determines,
            resolver,
            resolver_injective,
        } => {
            let decl = SurfaceDeclaration::ClassDecl {
                name: name.clone(),
                params: params.clone(),
                superclasses: superclasses.clone(),
                methods: methods
                    .iter()
                    .map(|e| Spanned::new(entry_to_surface(&e.node), e.span))
                    .collect(),
                determines: determines.iter().map(|e| expr_to_surface_node(e)).collect(),
                resolver: resolver.as_ref().map(|r| expr_to_surface_node(r)),
                resolver_injective: *resolver_injective,
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::InstanceDecl { class_name, arms } => {
            let decl = SurfaceDeclaration::InstanceDecl {
                class_name: class_name.clone(),
                arms: arms
                    .iter()
                    .map(|(pattern_expr, methods)| {
                        let surface_pattern = expr_to_surface_node(pattern_expr);
                        let surface_methods = methods
                            .iter()
                            .map(|e| Spanned::new(entry_to_surface(&e.node), e.span))
                            .collect();
                        (surface_pattern, surface_methods)
                    })
                    .collect(),
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::DefMacro { name, params, body } => {
            let decl = SurfaceDeclaration::DefMacro {
                name: name.clone(),
                params: expr_to_surface_node(params),
                body: expr_to_surface_node(body),
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::MacroDecl { name, params, body } => {
            let decl = SurfaceDeclaration::MacroDecl {
                name: name.clone(),
                params: expr_to_surface_node(params),
                body: expr_to_surface_node(body),
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::SyntaxClass {
            name,
            pattern,
            message,
        } => {
            let decl = SurfaceDeclaration::SyntaxClass {
                name: name.clone(),
                pattern: expr_to_surface_node(pattern),
                message: message.clone(),
            };
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }
        Expr::Splice(forms) => {
            let decl =
                SurfaceDeclaration::Splice(forms.iter().map(|e| expr_to_surface_node(e)).collect());
            SurfaceItem::Decl(Spanned::new(decl, spanned.span))
        }

        // All other forms → SurfaceItem::Expr
        _ => SurfaceItem::Expr(Arc::new(SurfaceNode {
            expr: expr_to_surface_expr(&spanned.node),
            span: spanned.span,
        })),
    }
}

/// Convert a `Spanned<Expr>` to an `Arc<SurfaceNode>`.
pub fn expr_to_surface_node(spanned: &Spanned<Expr>) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode {
        expr: expr_to_surface_expr(&spanned.node),
        span: spanned.span,
    })
}

/// Convert a `SurfaceNode` back to a `Spanned<Expr>` for unquote compatibility.
///
/// This is the reverse bridge — used when a `Value::Expression` is unquoted in a
/// `[quote ...]` context that expects an old `Expr` tree (e.g., macro expansion).
/// Deleted in Part E when the evaluator fully uses Surface types.
pub fn surface_node_to_expr(node: &Arc<SurfaceNode>) -> Spanned<Expr> {
    Spanned::new(surface_expr_to_expr(&node.expr, node.span), node.span)
}

fn surface_expr_to_expr(expr: &SurfaceExpression, _span: crate::ast::Span) -> Expr {
    use std::cell::RefCell;
    use std::rc::Rc;
    match expr {
        SurfaceExpression::Int(n) => Expr::Int(*n),
        SurfaceExpression::Float(n) => Expr::Float(*n),
        SurfaceExpression::Bool(b) => Expr::Bool(*b),
        SurfaceExpression::Str(s) => Expr::Str(s.clone()),
        SurfaceExpression::VarRef { name, escaped } => Expr::VarRef {
            name: name.clone(),
            escaped: *escaped,
            resolved: RefCell::new(None),
        },
        SurfaceExpression::DotAccess { expr: inner, field } => Expr::DotAccess {
            expr: Box::new(surface_node_to_expr(inner)),
            field: field.clone(),
        },
        SurfaceExpression::Pipe { lhs, rhs } => Expr::Pipe {
            lhs: Box::new(surface_node_to_expr(lhs)),
            rhs: Box::new(surface_node_to_expr(rhs)),
        },
        SurfaceExpression::Sequential(exprs) => Expr::Sequential(
            exprs
                .iter()
                .map(|e| Rc::new(surface_node_to_expr(e)))
                .collect(),
        ),
        SurfaceExpression::Dict(entries) => Expr::Dict(
            entries
                .iter()
                .map(|se| {
                    Spanned::new(
                        crate::ast::Entry {
                            key: se.node.key.as_ref().map(|k| surface_node_to_expr(k)),
                            value: Rc::new(surface_node_to_expr(&se.node.value)),
                        },
                        se.span,
                    )
                })
                .collect(),
        ),
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => Expr::Call {
            func: Box::new(surface_node_to_expr(func)),
            args: args
                .iter()
                .map(|a| Rc::new(surface_node_to_expr(a)))
                .collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    Spanned::new(
                        crate::ast::NamedArg {
                            name: na.node.name.clone(),
                            value: Rc::new(surface_node_to_expr(&na.node.value)),
                        },
                        na.span,
                    )
                })
                .collect(),
            implied: *implied,
        },
        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => Expr::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    Spanned::new(
                        crate::ast::Param {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span,
                    )
                })
                .collect(),
            body: Rc::new(surface_node_to_expr(body)),
            desugared: *desugared,
        },
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => Expr::TypeAssert {
            annotation: annotation.clone(),
            expr: Box::new(surface_node_to_expr(inner)),
            resolved_type: RefCell::new(None),
        },
        SurfaceExpression::Annotated { name, annotation } => Expr::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },
        SurfaceExpression::Rest(name) => Expr::Rest(name.clone()),
        SurfaceExpression::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(surface_node_to_expr(scrutinee)),
            arms: arms
                .iter()
                .map(|arm| crate::ast::MatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm
                        .guard
                        .as_ref()
                        .map(|g| Box::new(surface_node_to_expr(g))),
                    body: Box::new(surface_node_to_expr(&arm.body)),
                })
                .collect(),
        },
        SurfaceExpression::Quote(inner) => Expr::Quote(Box::new(surface_node_to_expr(inner))),
        SurfaceExpression::Unquote(inner) => Expr::Unquote(Box::new(surface_node_to_expr(inner))),
        SurfaceExpression::UnquoteSplice(inner) => {
            Expr::UnquoteSplice(Box::new(surface_node_to_expr(inner)))
        }
        SurfaceExpression::PatternDecl { bindings } => Expr::PatternDecl {
            bindings: bindings.iter().map(|b| surface_node_to_expr(b)).collect(),
        },
        SurfaceExpression::LetDecl { bindings } => Expr::LetDecl {
            bindings: bindings.iter().map(|b| surface_node_to_expr(b)).collect(),
        },
        SurfaceExpression::CaseArm { pattern, body } => Expr::CaseArm {
            pattern: Box::new(surface_node_to_expr(pattern)),
            body: Box::new(surface_node_to_expr(body)),
        },
        SurfaceExpression::TypeApp { func, arg } => Expr::TypeApp {
            func: Box::new(surface_node_to_expr(func)),
            arg: Box::new(surface_node_to_expr(arg)),
        },
        SurfaceExpression::Placeholder => Expr::Placeholder,
        SurfaceExpression::Error(s) => Expr::Error(*s),
    }
}

/// Convert a `Spanned<CoreExpr>` back to a `Spanned<Expr>` for transitional evaluation.
///
/// This is the bridge from CoreExpr → Expr, used during the transitional period when
/// eval_core_expr() falls back to the old Expr evaluation path for complex constructs.
/// As CoreExpr evaluation is built out, this function will be used less and eventually
/// deleted when all CoreExpr variants are handled directly.
pub fn core_expr_to_expr(core: &crate::ast::Spanned<crate::ast::CoreExpr>) -> Spanned<Expr> {
    Spanned::new(core_expr_inner_to_expr(&core.node), core.span)
}

fn core_expr_inner_to_expr(expr: &crate::ast::CoreExpr) -> Expr {
    use std::cell::RefCell;
    use std::rc::Rc;
    use crate::ast::CoreExpr;

    match expr {
        CoreExpr::Int(n) => Expr::Int(*n),
        CoreExpr::Float(n) => Expr::Float(*n),
        CoreExpr::Bool(b) => Expr::Bool(*b),
        CoreExpr::Str(s) => Expr::Str(s.clone()),

        // Var and FreeVar both become VarRef — the old AST doesn't have de Bruijn coordinates
        CoreExpr::Var { name, .. } => Expr::VarRef {
            name: name.clone(),
            escaped: false,
            resolved: RefCell::new(None),
        },
        CoreExpr::FreeVar(name) => Expr::VarRef {
            name: name.clone(),
            escaped: false,
            resolved: RefCell::new(None),
        },

        CoreExpr::DotAccess { expr: inner, field } => Expr::DotAccess {
            expr: Box::new(core_expr_to_expr(inner)),
            field: field.clone(),
        },

        // Note: CoreExpr has no Pipe variant (it's desugared to Call by lowering)
        CoreExpr::Sequential(exprs) => Expr::Sequential(
            exprs
                .iter()
                .map(|e| Rc::new(core_expr_to_expr(e)))
                .collect(),
        ),

        CoreExpr::Dict(entries) => Expr::Dict(
            entries
                .iter()
                .map(|ce| {
                    Spanned::new(
                        Entry {
                            key: ce.node.key.as_ref().map(|k| core_expr_to_expr(k)),
                            value: Rc::new(core_expr_to_expr(&ce.node.value)),
                        },
                        ce.span,
                    )
                })
                .collect(),
        ),

        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
        } => Expr::Call {
            func: Box::new(core_expr_to_expr(func)),
            args: args
                .iter()
                .map(|a| Rc::new(core_expr_to_expr(a)))
                .collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    Spanned::new(
                        crate::ast::NamedArg {
                            name: na.node.name.clone(),
                            value: Rc::new(core_expr_to_expr(&na.node.value)),
                        },
                        na.span,
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
        } => Expr::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    Spanned::new(
                        crate::ast::Param {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span,
                    )
                })
                .collect(),
            body: Rc::new(core_expr_to_expr(body)),
            desugared: *desugared,
        },

        CoreExpr::TypeAssert { annotation, expr: inner, .. } => Expr::TypeAssert {
            annotation: annotation.clone(),
            expr: Box::new(core_expr_to_expr(inner)),
            resolved_type: RefCell::new(None),
        },

        CoreExpr::RuntimeTypeCheck { annotation, expr: inner, .. } => Expr::TypeAssert {
            annotation: annotation.clone(),
            expr: Box::new(core_expr_to_expr(inner)),
            resolved_type: RefCell::new(None),
        },

        CoreExpr::Annotated { name, annotation } => Expr::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },

        CoreExpr::Rest(name) => Expr::Rest(name.clone()),

        CoreExpr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(core_expr_to_expr(scrutinee)),
            arms: arms
                .iter()
                .map(|arm| crate::ast::MatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm
                        .guard
                        .as_ref()
                        .map(|g| Box::new(core_expr_to_expr(g))),
                    body: Box::new(core_expr_to_expr(&arm.body)),
                })
                .collect(),
        },

        CoreExpr::Quote(inner) => Expr::Quote(Box::new(core_expr_to_expr(inner))),
        CoreExpr::Unquote(inner) => Expr::Unquote(Box::new(core_expr_to_expr(inner))),
        CoreExpr::UnquoteSplice(inner) => Expr::UnquoteSplice(Box::new(core_expr_to_expr(inner))),

        CoreExpr::PatternDecl { bindings } => Expr::PatternDecl {
            bindings: bindings.iter().map(|b| core_expr_to_expr(b)).collect(),
        },

        CoreExpr::LetDecl { bindings } => Expr::LetDecl {
            bindings: bindings.iter().map(|b| core_expr_to_expr(b)).collect(),
        },

        CoreExpr::CaseArm { pattern, body } => Expr::CaseArm {
            pattern: Box::new(core_expr_to_expr(pattern)),
            body: Box::new(core_expr_to_expr(body)),
        },

        CoreExpr::TypeApp { func, arg } => Expr::TypeApp {
            func: Box::new(core_expr_to_expr(func)),
            arg: Box::new(core_expr_to_expr(arg)),
        },

        CoreExpr::Placeholder => Expr::Placeholder,
        CoreExpr::Error(s) => Expr::Error(*s),
    }
}

fn expr_to_surface_expr(expr: &Expr) -> SurfaceExpression {
    match expr {
        Expr::Int(n) => SurfaceExpression::Int(*n),
        Expr::Float(n) => SurfaceExpression::Float(*n),
        Expr::Bool(b) => SurfaceExpression::Bool(*b),
        Expr::Str(s) => SurfaceExpression::Str(s.clone()),

        Expr::VarRef { name, escaped, .. } => SurfaceExpression::VarRef {
            name: name.clone(),
            escaped: *escaped,
        },

        Expr::DotAccess { expr, field } => SurfaceExpression::DotAccess {
            expr: expr_to_surface_node(expr),
            field: field.clone(),
        },

        Expr::Pipe { lhs, rhs } => SurfaceExpression::Pipe {
            lhs: expr_to_surface_node(lhs),
            rhs: expr_to_surface_node(rhs),
        },

        Expr::Sequential(exprs) => {
            SurfaceExpression::Sequential(exprs.iter().map(|e| expr_to_surface_node(e)).collect())
        }

        Expr::Dict(entries) => SurfaceExpression::Dict(
            entries
                .iter()
                .map(|e| Spanned::new(entry_to_surface(&e.node), e.span))
                .collect(),
        ),

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => SurfaceExpression::Call {
            func: expr_to_surface_node(func),
            args: args.iter().map(|a| expr_to_surface_node(a)).collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    Spanned::new(
                        SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: expr_to_surface_node(&na.node.value),
                        },
                        na.span,
                    )
                })
                .collect(),
            implied: *implied,
        },

        Expr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => SurfaceExpression::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    Spanned::new(
                        SurfaceParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span,
                    )
                })
                .collect(),
            body: expr_to_surface_node(body),
            desugared: *desugared,
        },

        Expr::TypeAssert {
            annotation, expr, ..
        } => SurfaceExpression::TypeAssert {
            annotation: annotation.clone(),
            expr: expr_to_surface_node(expr),
        },

        Expr::Annotated { name, annotation } => SurfaceExpression::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },

        Expr::Rest(name) => SurfaceExpression::Rest(name.clone()),

        Expr::Match { scrutinee, arms } => SurfaceExpression::Match {
            scrutinee: expr_to_surface_node(scrutinee),
            arms: arms.iter().map(|arm| match_arm_to_surface(arm)).collect(),
        },

        Expr::Quote(inner) => SurfaceExpression::Quote(expr_to_surface_node(inner)),
        Expr::Unquote(inner) => SurfaceExpression::Unquote(expr_to_surface_node(inner)),
        Expr::UnquoteSplice(inner) => SurfaceExpression::UnquoteSplice(expr_to_surface_node(inner)),

        Expr::PatternDecl { bindings } => SurfaceExpression::PatternDecl {
            bindings: bindings.iter().map(|b| expr_to_surface_node(b)).collect(),
        },

        Expr::LetDecl { bindings } => SurfaceExpression::LetDecl {
            bindings: bindings.iter().map(|b| expr_to_surface_node(b)).collect(),
        },

        Expr::CaseArm { pattern, body } => SurfaceExpression::CaseArm {
            pattern: expr_to_surface_node(pattern),
            body: expr_to_surface_node(body),
        },

        Expr::TypeApp { func, arg } => SurfaceExpression::TypeApp {
            func: expr_to_surface_node(func),
            arg: expr_to_surface_node(arg),
        },

        Expr::Placeholder => SurfaceExpression::Placeholder,
        Expr::Error(span) => SurfaceExpression::Error(*span),

        // Declaration forms should have been filtered out by expr_to_surface_item.
        // If they appear in a non-item position (e.g., nested in a call), convert
        // to Placeholder so the lowering pass can raise a proper error at force time.
        Expr::TypeAlias { .. }
        | Expr::ClassDecl { .. }
        | Expr::InstanceDecl { .. }
        | Expr::DefMacro { .. }
        | Expr::MacroDecl { .. }
        | Expr::SyntaxClass { .. }
        | Expr::Splice(_) => SurfaceExpression::Placeholder,
    }
}

fn entry_to_surface(entry: &Entry) -> SurfaceEntry {
    SurfaceEntry {
        key: entry.key.as_ref().map(|k| expr_to_surface_node(k)),
        value: expr_to_surface_node(&entry.value),
    }
}

fn match_arm_to_surface(arm: &MatchArm) -> SurfaceMatchArm {
    SurfaceMatchArm {
        pattern: arm.pattern.clone(),
        guard: arm.guard.as_ref().map(|g| expr_to_surface_node(g)),
        body: expr_to_surface_node(&arm.body),
    }
}

/// Parse tinct source and return a `SurfaceProgram`.
///
/// This is a temporary bridge until the parser directly produces `SurfaceProgram`.
/// Currently converts the parsed File to SurfaceProgram.
pub fn parse_to_surface(
    input: &str,
) -> Result<(SurfaceProgram, crate::parser::ParseOutput), crate::parser::ParseError> {
    let output = crate::parser::parse(input)?;
    let program = file_to_surface_program(&output.file.node);
    Ok((program, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn test_convert_empty_file() {
        let output = parse("").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        // Parser always produces one document (may be empty); empty input → one empty doc
        assert_eq!(program.documents.len(), 1);
        assert!(program.documents[0].node.items.is_empty());
    }

    #[test]
    fn test_convert_int_literal() {
        let output = parse("42").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        assert_eq!(program.documents.len(), 1);
        let doc = &program.documents[0].node;
        assert_eq!(doc.items.len(), 1);
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(node.expr, SurfaceExpression::Int(42)));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_varref() {
        let output = parse("x").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    SurfaceExpression::VarRef { name, escaped: false } if name == "x"
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_call() {
        let output = parse("[+ 1 2]").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    SurfaceExpression::Call { args, implied: true, .. } if args.len() == 2
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_dict() {
        let output = parse("[a: 1  b: 2]").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    SurfaceExpression::Dict(entries) if entries.len() == 2
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_type_alias_is_decl() {
        let output = parse("[type Int]").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        let doc = &program.documents[0].node;
        assert!(matches!(&doc.items[0], SurfaceItem::Decl(_)));
    }

    #[test]
    fn test_convert_fn() {
        let output = parse("[fn [x y] [+ x y]]").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    SurfaceExpression::Fn { params, .. } if params.len() == 2
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_multi_document() {
        let output = parse("a\n---\nb").expect("parse failed");
        let program = file_to_surface_program(&output.file.node);
        assert_eq!(program.documents.len(), 2);
    }
}
