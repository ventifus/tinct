//! AST conversion bridges for incremental migrations.
//!
//! This module contains THREE bridge functions for different migration phases:
//!
//! 1. **file_to_surface_program** (File → SurfaceProgram)
//!    - Converts old parser output to Surface AST types
//!    - Deleted when parser is migrated to produce SurfaceProgram directly
//!
//! 2. **expr_to_core_expr** (Expr → CoreExpr) — FORWARD BRIDGE
//!    - Converts old Expr to CoreExpr for E1-eval-cutover
//!    - Used by eval_recursive to route all evaluation through CoreExpr
//!    - Deleted when all Value/thunk types are migrated to CoreExpr (post-E3)
//!
//! 3. **core_expr_to_expr** (CoreExpr → Expr) — REVERSE BRIDGE
//!    - Converts CoreExpr back to Expr for transitional compatibility
//!    - Used by eval_core_expr to call existing helpers (eval_dict, eval_call, etc.)
//!    - Deleted when eval_dict/eval_call/etc. are refactored to accept CoreExpr (E2/E3)
//!
//! All three bridges are TRANSITIONAL and will be deleted as migrations complete.

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
/// Requires that type checking has already been run on the file so that
/// `TypeAssert.resolved_type` RefCells are populated. During the bridge conversion,
/// each `TypeAssert` node in the old File that has a resolved type is recorded in
/// the table keyed by the corresponding `SurfaceNode`'s `NodeId`.
///
/// Deleted in Part E when the typechecker directly produces `TypeAnnotationTable`.
#[allow(dead_code)] // Used in Part B when typechecker integration lands
pub fn file_to_surface_program_with_types(file: &File) -> (SurfaceProgram, TypeAnnotationTable) {
    let mut table = TypeAnnotationTable::new();
    let program = file_to_surface_program_collecting(file, &mut table);
    (program, table)
}

#[allow(dead_code)] // Helper for file_to_surface_program_with_types (Part B)
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

#[allow(dead_code)] // Helper for file_to_surface_program_with_types (Part B)
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

#[allow(dead_code)] // Helper for file_to_surface_program_with_types (Part B)
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

#[allow(dead_code)] // Helper for file_to_surface_program_with_types (Part B)
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
    use crate::ast::CoreExpr;
    use std::cell::RefCell;
    use std::rc::Rc;

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
            args: args.iter().map(|a| Rc::new(core_expr_to_expr(a))).collect(),
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

        CoreExpr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => Expr::TypeAssert {
            annotation: annotation.clone(),
            expr: Box::new(core_expr_to_expr(inner)),
            resolved_type: RefCell::new(Some(resolved_type.clone())),
        },

        CoreExpr::RuntimeTypeCheck {
            annotation,
            expr: inner,
            ..
        } => Expr::TypeAssert {
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
                    guard: arm.guard.as_ref().map(|g| Box::new(core_expr_to_expr(g))),
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

/// Convert a `Spanned<Expr>` to a `Spanned<CoreExpr>` for the eval cutover.
///
/// This is the forward bridge from Expr → CoreExpr, used during the E1-eval-cutover
/// sprint to route all evaluation through `eval_core_expr()`. Once all callers have
/// been migrated to work with CoreExpr directly, this bridge can be deleted.
///
/// Key transformations:
/// - VarRef with resolved (level, slot) → Var with de Bruijn coordinates
/// - VarRef with unresolved → FreeVar (name-based lookup)
/// - Pipe → Call (pipe is sugar for implied call)
/// - TypeAssert with resolved_type → TypeAssert or RuntimeTypeCheck based on presence
pub fn expr_to_core_expr(expr: &Spanned<Expr>) -> Spanned<crate::ast::CoreExpr> {
    Spanned::new(expr_inner_to_core_expr(&expr.node, expr.span), expr.span)
}

fn expr_inner_to_core_expr(expr: &Expr, span: crate::ast::Span) -> crate::ast::CoreExpr {
    use crate::ast::{CoreEntry, CoreExpr, CoreMatchArm, CoreNamedArg, CoreParam};

    match expr {
        Expr::Int(n) => CoreExpr::Int(*n),
        Expr::Float(f) => CoreExpr::Float(*f),
        Expr::Bool(b) => CoreExpr::Bool(*b),
        Expr::Str(s) => CoreExpr::Str(s.clone()),

        Expr::VarRef { name, resolved, .. } => {
            // Check if we have resolved de Bruijn coordinates
            if let Some(Some((level, slot))) = *resolved.borrow() {
                CoreExpr::Var {
                    name: name.clone(),
                    level,
                    slot,
                }
            } else {
                // Unresolved or unresolvable → FreeVar
                CoreExpr::FreeVar(name.clone())
            }
        }

        Expr::DotAccess { expr: inner, field } => CoreExpr::DotAccess {
            expr: Arc::new(expr_to_core_expr(inner)),
            field: field.clone(),
        },

        // Pipe is desugared to Call during conversion
        Expr::Pipe { lhs, rhs } => CoreExpr::Call {
            func: Arc::new(expr_to_core_expr(rhs)),
            args: vec![Arc::new(expr_to_core_expr(lhs))],
            named_args: vec![],
            implied: true,
        },

        Expr::Sequential(exprs) => CoreExpr::Sequential(
            exprs
                .iter()
                .map(|e| Arc::new(expr_to_core_expr(e)))
                .collect(),
        ),

        Expr::Dict(entries) => CoreExpr::Dict(
            entries
                .iter()
                .map(|e| {
                    Spanned::new(
                        CoreEntry {
                            key: e.node.key.as_ref().map(|k| Arc::new(expr_to_core_expr(k))),
                            value: Arc::new(expr_to_core_expr(&e.node.value)),
                        },
                        e.span,
                    )
                })
                .collect(),
        ),

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => CoreExpr::Call {
            func: Arc::new(expr_to_core_expr(func)),
            args: args
                .iter()
                .map(|a| Arc::new(expr_to_core_expr(a)))
                .collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    Spanned::new(
                        CoreNamedArg {
                            name: na.node.name.clone(),
                            value: Arc::new(expr_to_core_expr(&na.node.value)),
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
                        p.span,
                    )
                })
                .collect(),
            body: Arc::new(expr_to_core_expr(body)),
            desugared: *desugared,
        },

        Expr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => {
            // Check if typechecker provided a resolved type
            if let Some(resolved) = resolved_type.borrow().clone() {
                CoreExpr::TypeAssert {
                    annotation: annotation.clone(),
                    expr: Arc::new(expr_to_core_expr(inner)),
                    resolved_type: resolved,
                }
            } else {
                // No resolved type — runtime type check with optional default
                let default = annotation
                    .node
                    .get_property("default")
                    .map(|e| Arc::new(expr_to_core_expr(&e)));
                CoreExpr::RuntimeTypeCheck {
                    annotation: annotation.clone(),
                    expr: Arc::new(expr_to_core_expr(inner)),
                    default,
                }
            }
        }

        Expr::Annotated { name, annotation } => CoreExpr::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },

        Expr::Rest(name) => CoreExpr::Rest(name.clone()),

        Expr::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(expr_to_core_expr(scrutinee)),
            arms: arms
                .iter()
                .map(|arm| CoreMatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(|g| Arc::new(expr_to_core_expr(g))),
                    body: Arc::new(expr_to_core_expr(&arm.body)),
                })
                .collect(),
        },

        Expr::Quote(inner) => CoreExpr::Quote(Arc::new(expr_to_core_expr(inner))),
        Expr::Unquote(inner) => CoreExpr::Unquote(Arc::new(expr_to_core_expr(inner))),
        Expr::UnquoteSplice(inner) => CoreExpr::UnquoteSplice(Arc::new(expr_to_core_expr(inner))),

        Expr::PatternDecl { bindings } => CoreExpr::PatternDecl {
            bindings: bindings.iter().map(|b| expr_to_core_expr(b)).collect(),
        },

        Expr::LetDecl { bindings } => CoreExpr::LetDecl {
            bindings: bindings.iter().map(|b| expr_to_core_expr(b)).collect(),
        },

        Expr::CaseArm { pattern, body } => CoreExpr::CaseArm {
            pattern: Arc::new(expr_to_core_expr(pattern)),
            body: Arc::new(expr_to_core_expr(body)),
        },

        Expr::TypeApp { func, arg } => CoreExpr::TypeApp {
            func: Arc::new(expr_to_core_expr(func)),
            arg: Arc::new(expr_to_core_expr(arg)),
        },

        Expr::Placeholder => CoreExpr::Placeholder,

        Expr::Error(span) => CoreExpr::Error(*span),

        // TypeAlias, ClassDecl, InstanceDecl, DefMacro, MacroDecl, SyntaxClass, Splice
        // are declaration forms that should not be evaluated. Convert to Error with
        // proper span so error messages point to the declaration site.
        Expr::TypeAlias { .. } => CoreExpr::Error(span),
        Expr::ClassDecl { .. } => CoreExpr::Error(span),
        Expr::InstanceDecl { .. } => CoreExpr::Error(span),
        Expr::DefMacro { .. } => CoreExpr::Error(span),
        Expr::MacroDecl { .. } => CoreExpr::Error(span),
        Expr::SyntaxClass { .. } => CoreExpr::Error(span),
        Expr::Splice(_) => CoreExpr::Error(span),
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

/// Convert a `SurfaceDeclaration` back to the corresponding `Spanned<Expr>`.
///
/// This is the reverse of `expr_to_surface_item` for declaration forms.
/// Used by `parse_expression()` to handle top-level declaration items.
/// Deleted in Part E when `parse_expression()` is retired.
pub fn surface_decl_to_expr(decl: &Spanned<SurfaceDeclaration>) -> Spanned<Expr> {
    use std::rc::Rc;

    let expr = match &decl.node {
        SurfaceDeclaration::TypeAlias { params, body } => Expr::TypeAlias {
            params: params.clone(),
            body: Box::new(surface_node_to_expr(body)),
        },
        SurfaceDeclaration::ClassDecl {
            name,
            params,
            superclasses,
            methods,
            determines,
            resolver,
            resolver_injective,
        } => Expr::ClassDecl {
            name: name.clone(),
            params: params.clone(),
            superclasses: superclasses.clone(),
            methods: methods
                .iter()
                .map(|se| {
                    Spanned::new(
                        Entry {
                            key: se.node.key.as_ref().map(|k| surface_node_to_expr(k)),
                            value: Rc::new(surface_node_to_expr(&se.node.value)),
                        },
                        se.span,
                    )
                })
                .collect(),
            determines: determines.iter().map(|n| surface_node_to_expr(n)).collect(),
            resolver: resolver.as_ref().map(|r| Box::new(surface_node_to_expr(r))),
            resolver_injective: *resolver_injective,
        },
        SurfaceDeclaration::InstanceDecl { class_name, arms } => Expr::InstanceDecl {
            class_name: class_name.clone(),
            arms: arms
                .iter()
                .map(|(pattern, methods)| {
                    let old_pattern = surface_node_to_expr(pattern);
                    let old_methods = methods
                        .iter()
                        .map(|se| {
                            Spanned::new(
                                Entry {
                                    key: se.node.key.as_ref().map(|k| surface_node_to_expr(k)),
                                    value: Rc::new(surface_node_to_expr(&se.node.value)),
                                },
                                se.span,
                            )
                        })
                        .collect();
                    (old_pattern, old_methods)
                })
                .collect(),
        },
        SurfaceDeclaration::DefMacro { name, params, body } => Expr::DefMacro {
            name: name.clone(),
            params: Rc::new(surface_node_to_expr(params)),
            body: Rc::new(surface_node_to_expr(body)),
        },
        SurfaceDeclaration::MacroDecl { name, params, body } => Expr::MacroDecl {
            name: name.clone(),
            params: Box::new(surface_node_to_expr(params)),
            body: Box::new(surface_node_to_expr(body)),
        },
        SurfaceDeclaration::SyntaxClass {
            name,
            pattern,
            message,
        } => Expr::SyntaxClass {
            name: name.clone(),
            pattern: Box::new(surface_node_to_expr(pattern)),
            message: message.clone(),
        },
        SurfaceDeclaration::Splice(forms) => {
            Expr::Splice(forms.iter().map(|n| surface_node_to_expr(n)).collect())
        }
    };
    Spanned::new(expr, decl.span)
}

fn match_arm_to_surface(arm: &MatchArm) -> SurfaceMatchArm {
    SurfaceMatchArm {
        pattern: arm.pattern.clone(),
        guard: arm.guard.as_ref().map(|g| expr_to_surface_node(g)),
        body: expr_to_surface_node(&arm.body),
    }
}

/// Convert a `SurfaceProgram` back to a `Spanned<File>` for compatibility with
/// passes that still consume the old AST (desugar, resolve, typecheck, eval, expand).
///
/// This is the reverse of `file_to_surface_program()`. Deleted in Part E when
/// all downstream passes are migrated to consume `SurfaceProgram` directly.
pub fn surface_program_to_file(program: &SurfaceProgram) -> Spanned<File> {
    use std::rc::Rc;

    let documents = program
        .documents
        .iter()
        .map(|surface_doc| {
            let doc_node = &surface_doc.node;
            let expressions: Vec<Rc<Spanned<Expr>>> = doc_node
                .items
                .iter()
                .filter_map(|item| match item {
                    SurfaceItem::Expr(node) => Some(Rc::new(surface_node_to_expr(node))),
                    SurfaceItem::Decl(_) => None,
                })
                .collect();

            Spanned::new(
                Document {
                    expressions,
                    name: doc_node.name.clone(),
                    output_type: doc_node.output_type.clone(),
                    expects: doc_node.expects.clone(),
                    caps: doc_node.caps.clone(),
                    stage: doc_node.stage.clone(),
                },
                surface_doc.span,
            )
        })
        .collect();

    // Use a zero-width span; callers only care about the inner File.
    Spanned::new(
        File { documents },
        crate::ast::Span {
            start: crate::ast::Position {
                line: 1,
                column: 1,
                offset: 0,
            },
            end: crate::ast::Position {
                line: 1,
                column: 1,
                offset: 0,
            },
        },
    )
}

/// Parse tinct source and return a `SurfaceProgram`.
///
/// This is a temporary bridge until the parser directly produces `SurfaceProgram`.
/// Currently converts the parsed File to SurfaceProgram.
#[allow(dead_code)] // Used in Part B when parser integration changes
pub fn parse_to_surface(
    input: &str,
) -> Result<(SurfaceProgram, crate::parser::ParseOutput), crate::parser::ParseError> {
    let output = crate::parser::parse(input)?;
    let program = output.program.clone();
    Ok((program, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::parser::parse;

    #[test]
    fn test_convert_empty_file() {
        let output = parse("").expect("parse failed");
        let program = output.program.clone();
        // Parser always produces one document (may be empty); empty input → one empty doc
        assert_eq!(program.documents.len(), 1);
        assert!(program.documents[0].node.items.is_empty());
    }

    #[test]
    fn test_convert_int_literal() {
        let output = parse("42").expect("parse failed");
        let program = output.program.clone();
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
        let program = output.program.clone();
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
        let program = output.program.clone();
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
        let program = output.program.clone();
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
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        assert!(matches!(&doc.items[0], SurfaceItem::Decl(_)));
    }

    #[test]
    fn test_convert_fn() {
        let output = parse("[fn [let x y] [+ x y]]").expect("parse failed");
        let program = output.program.clone();
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
        let program = output.program.clone();
        assert_eq!(program.documents.len(), 2);
    }

    // Test coverage for the 7 simple Expr → SurfaceExpression variants (parser-migration-a scope)

    #[test]
    fn test_convert_float_literal() {
        let output = parse("3.14").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(
                    matches!(node.expr, SurfaceExpression::Float(f) if (f - 3.14).abs() < 1e-10)
                );
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_bool_literal() {
        let output = parse("true").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(node.expr, SurfaceExpression::Bool(true)));
            }
            _ => panic!("expected Expr item"),
        }

        let output = parse("false").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(node.expr, SurfaceExpression::Bool(false)));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_str_literal() {
        let output = parse(r#""hello world""#).expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(&node.expr, SurfaceExpression::Str(s) if s == "hello world"));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_escaped_varref() {
        let output = parse("$escaped-name").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(
                    &node.expr,
                    SurfaceExpression::VarRef { name, escaped: true } if name == "escaped-name"
                ));
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_rest_named() {
        // Rest with a name: `[a: 1 ...rest]`
        let output = parse("[a: 1 ...rest]").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                if let SurfaceExpression::Dict(entries) = &node.expr {
                    assert_eq!(entries.len(), 2);
                    // Second entry should be the rest marker
                    match &entries[1].node.value.expr {
                        SurfaceExpression::Rest(Some(name)) => assert_eq!(name, "rest"),
                        _ => panic!("expected Rest(Some(name))"),
                    }
                } else {
                    panic!("expected Dict");
                }
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_rest_anonymous() {
        // Rest without a name (open row): `[a: 1 ...]`
        let output = parse("[a: 1 ...]").expect("parse failed");
        let program = output.program.clone();
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                if let SurfaceExpression::Dict(entries) = &node.expr {
                    assert_eq!(entries.len(), 2);
                    // Second entry should be the anonymous rest marker
                    match &entries[1].node.value.expr {
                        SurfaceExpression::Rest(None) => {}
                        _ => panic!("expected Rest(None)"),
                    }
                } else {
                    panic!("expected Dict");
                }
            }
            _ => panic!("expected Expr item"),
        }
    }

    #[test]
    fn test_convert_placeholder() {
        // Currently no direct syntax for Placeholder in the parser;
        // it's used internally in some contexts. This test documents
        // that the bridge handles it correctly.
        use std::rc::Rc;
        let file = File {
            documents: vec![Spanned::new(
                Document {
                    stage: None,
                    name: None,
                    expressions: vec![Rc::new(Spanned::new(Expr::Placeholder, Span::origin()))],
                    output_type: None,
                    expects: None,
                    caps: None,
                },
                Span::origin(),
            )],
        };
        let program = file_to_surface_program(&file);
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => {
                assert!(matches!(node.expr, SurfaceExpression::Placeholder));
            }
            _ => panic!("expected Expr item"),
        }
    }
}
