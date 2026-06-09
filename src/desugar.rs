//! `$_` desugaring and Pipe lowering — pre-typecheck AST transformations.
//!
//! This module performs two source-to-source transformations on `SurfaceProgram`
//! and `Arc<SurfaceNode>`:
//!
//! 1. **`$_` → implicit lambda desugaring.** Rewrites expressions containing
//!    `$_` (underscore placeholder) into explicit lambda expressions using the
//!    DIRECT/WRAP rules. For example, `[add _ 1]` becomes `[fn [_] [add _ 1]]`.
//!    The transformation runs after parsing and before type checking and evaluation.
//!
//! 2. **`Pipe(lhs, rhs)` → `Call(rhs, [lhs])` lowering.** All `Pipe` nodes are
//!    rewritten to `Call` nodes during desugaring — this applies to *every* pipe
//!    expression, not just those involving `$_`. After this pass, `Expr::Pipe` nodes
//!    produced by the desugar pass are converted to `Call` nodes. The type checker
//!    asserts `Expr::Pipe` is unreachable (typecheck.rs); the evaluator
//!    eliminates them via `expr_to_core_expr` in `ast_convert.rs` (which converts
//!    Pipe to Call silently). This module is the single lowering site for pipe expressions.
//!
//! **Desugar nesting depth invariant:** Desugar only transforms `$_` into fn wrappers
//! (one level per `$_` occurrence). Nesting depth is bounded by the parser's
//! MAX_PARSE_DEPTH (256), replaced by MAX_CONTINUATION_STACK (2048) in the CEK machine.
//! Therefore, desugaring cannot produce ASTs deeper than the parse depth limit.
//!
//! See doc/04-functions.md §`$_` Desugaring for the complete formal specification.

use crate::ast::{
    Annotation, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode, SurfaceParam,
    SurfaceProgram,
};
use std::sync::Arc;

/// Desugar a complete SurfaceProgram (all documents).
///
/// Runs the `$_` desugaring transformation on every expression in every document.
/// Mutates the SurfaceProgram in place by replacing SurfaceItem::Expr nodes.
pub fn desugar_surface_program(program: &mut SurfaceProgram) {
    for doc_spanned in &mut program.documents {
        desugar_surface_document(&mut doc_spanned.node);
    }
}

/// Inject constructor bindings into dicts that contain `[type ...]` declarations.
///
/// For each dict entry whose value is a `SurfaceExpression::Decl(TypeAlias)`, this pass
/// inspects the TypeAlias body, classifies each constructor, and injects synthetic dict
/// entries BEFORE the TypeAlias entry. The injected entries use the **unqualified** constructor
/// name as the dict key (so `Circle` is accessible as `dict.Circle`), but the variant tag
/// stored inside is **qualified** as `"TypeName.CtorName"` for disambiguation in pattern
/// matching (T-974).
///
/// Constructor classification:
/// - Bare uppercase `VarRef { name }` in the body → unit constructor.
///   Injects `"CtorName": [variant "TypeName.CtorName"]`.
/// - `Call { func: VarRef(UpperName), named_args: [...], ... }` with non-empty named_args →
///   named-field constructor. Injects a variadic `fn` wrapper:
///   `"CtorName": [fn [...payload] [variant-payload "TypeName.CtorName" payload]]`.
///   The variadic `...payload` param collects all named args into a Dict (via B-277 extension
///   to bind_args_thunks). Users call constructors with named args:
///   `[Ctor field: val]` → `Variant { tag: "TypeName.CtorName", payload: {field: val} }`.
/// - `Call { func: VarRef(UpperName), args: [...], named_args: [] }` (positional-only args
///   or zero args) → unit constructor. The positional args are type-variable annotations
///   in the type body, not runtime field names. Injects as a unit variant.
///
/// Type name extraction: `se.node.key` must be `SurfaceExpression::Str(s)` or
/// `SurfaceExpression::VarRef { name }`. Computed keys and absent keys are handled
/// gracefully: computed keys (e.g., `[fn [] "k"]`) are skipped (no injection for that
/// entry); absent keys (positional type declarations) use unqualified tags (no prefix).
///
/// This runs BEFORE `resolve_surface_program`, so the resolver correctly assigns de Bruijn
/// slots to the injected constructor names.
///
/// The TypeAlias Decl entry itself is preserved so the type checker can still register it.
/// At runtime, the Decl entry lowers to `CoreExpr::Placeholder` and is skipped by lower.rs.
pub fn inject_adt_constructors_surface_program(program: &mut SurfaceProgram) {
    for doc_spanned in &mut program.documents {
        inject_adt_constructors_document(&mut doc_spanned.node);
    }
}

/// Transform InstanceDecl in dict-entry position into runtime method dicts (T-1142).
///
/// For each dict entry whose value is a `SurfaceExpression::Decl(InstanceDecl)`, this pass
/// extracts the instance methods from the first arm and replaces the Decl with an explicit
/// `SurfaceExpression::Dict` containing those methods. This enables runtime method access
/// (e.g., `MonadResult.bind`) without special-casing InstanceDecl in the lowering pass.
///
/// **Single-arm assumption:** InstanceDecl lowering here only fires for instances that appear
/// as dict entry VALUES (expression position), e.g.:
///   `MonadResult: [instance Monad [let m@Result]: [bind: ...]]`
/// All such instances in the prelude have exactly one `[let ...]` arm, so arms[0] is always
/// the correct and only arm.
///
/// Multi-arm instances (e.g., `Addable` with four `[let a@T b@U c]` arms) are declared at
/// the top level as `SurfaceItem::Decl`, not as dict entry values, and therefore never reach
/// this code path. They remain as Decl nodes and lower to `CoreExpr::Placeholder`.
///
/// Runs AFTER `inject_adt_constructors_surface_program` (which also transforms dict entries)
/// and BEFORE `desugar_surface_program` (`$_` desugaring and pipe lowering).
pub fn desugar_instance_decls_surface_program(program: &mut SurfaceProgram) {
    for doc_spanned in &mut program.documents {
        desugar_instance_decls_document(&mut doc_spanned.node);
    }
}

fn desugar_instance_decls_document(doc: &mut SurfaceDocument) {
    for item in &mut doc.items {
        if let SurfaceItem::Expr(node_arc) = item {
            let new_node = desugar_instance_decls_node(Arc::clone(node_arc));
            *node_arc = new_node;
        }
    }
}

fn desugar_instance_decls_node(node: Arc<SurfaceNode>) -> Arc<SurfaceNode> {
    let span = node.span.clone();
    let new_expr = desugar_instance_decls_expr(&node.expr, span.clone());
    if std::ptr::eq(&new_expr as *const _, &node.expr as *const _) {
        // No change — return original Arc (avoids clone)
        return node;
    }
    Arc::new(SurfaceNode {
        expr: new_expr,
        span,
    })
}

fn desugar_instance_decls_expr(expr: &SurfaceExpression, _span: Span) -> SurfaceExpression {
    match expr {
        SurfaceExpression::Dict(entries) => {
            let mut new_entries: Vec<Spanned<SurfaceEntry>> = Vec::new();
            let mut has_transformation = false;

            for se in entries {
                // Check if this entry's value is an InstanceDecl
                let transformed_value = if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    if let SurfaceDeclaration::InstanceDecl { arms, .. } = decl.as_ref() {
                        if !arms.is_empty() {
                            // Transform: extract methods from arms[0].1 and build a Dict
                            has_transformation = true;
                            let method_entries = &arms[0].1;
                            // Recurse into method entries to handle nested instances
                            let desugared_methods: Vec<Spanned<SurfaceEntry>> = method_entries
                                .iter()
                                .map(|me| {
                                    let new_key = me
                                        .node
                                        .key
                                        .as_ref()
                                        .map(|k| desugar_instance_decls_node(Arc::clone(k)));
                                    let new_value =
                                        desugar_instance_decls_node(Arc::clone(&me.node.value));
                                    Spanned::new(
                                        SurfaceEntry {
                                            key: new_key,
                                            value: new_value,
                                        },
                                        me.span.clone(),
                                    )
                                })
                                .collect();
                            Some(Arc::new(SurfaceNode {
                                expr: SurfaceExpression::Dict(desugared_methods),
                                span: se.node.value.span.clone(),
                            }))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(new_value) = transformed_value {
                    // Entry was transformed: use new method dict
                    let new_key = se
                        .node
                        .key
                        .as_ref()
                        .map(|k| desugar_instance_decls_node(Arc::clone(k)));
                    new_entries.push(Spanned::new(
                        SurfaceEntry {
                            key: new_key,
                            value: new_value,
                        },
                        se.span.clone(),
                    ));
                } else {
                    // Entry not transformed: recurse into key and value
                    let new_value = desugar_instance_decls_node(Arc::clone(&se.node.value));
                    let new_key = se
                        .node
                        .key
                        .as_ref()
                        .map(|k| desugar_instance_decls_node(Arc::clone(k)));
                    new_entries.push(Spanned::new(
                        SurfaceEntry {
                            key: new_key,
                            value: new_value,
                        },
                        se.span.clone(),
                    ));
                }
            }

            if has_transformation {
                SurfaceExpression::Dict(new_entries)
            } else {
                // No transformations: check if recursion changed anything
                let changed = new_entries.iter().zip(entries.iter()).any(|(new, old)| {
                    !Arc::ptr_eq(&new.node.value, &old.node.value)
                        || new
                            .node
                            .key
                            .as_ref()
                            .zip(old.node.key.as_ref())
                            .is_some_and(|(a, b)| !Arc::ptr_eq(a, b))
                });
                if changed {
                    SurfaceExpression::Dict(new_entries)
                } else {
                    expr.clone()
                }
            }
        }
        // Recurse into other expression types that can contain dicts
        SurfaceExpression::Sequential(exprs) => {
            let new_exprs: Vec<Arc<SurfaceNode>> = exprs
                .iter()
                .map(|e| desugar_instance_decls_node(Arc::clone(e)))
                .collect();
            let changed = new_exprs
                .iter()
                .zip(exprs.iter())
                .any(|(a, b)| !Arc::ptr_eq(a, b));
            if changed {
                SurfaceExpression::Sequential(new_exprs)
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            let new_func = desugar_instance_decls_node(Arc::clone(func));
            let new_args: Vec<Arc<SurfaceNode>> = args
                .iter()
                .map(|a| desugar_instance_decls_node(Arc::clone(a)))
                .collect();
            let new_named: Vec<Spanned<SurfaceNamedArg>> = named_args
                .iter()
                .map(|na| {
                    let new_val = desugar_instance_decls_node(Arc::clone(&na.node.value));
                    Spanned::new(
                        SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: new_val,
                            annotation: na.node.annotation.clone(),
                        },
                        na.span.clone(),
                    )
                })
                .collect();
            let changed = !Arc::ptr_eq(&new_func, func)
                || new_args
                    .iter()
                    .zip(args.iter())
                    .any(|(a, b)| !Arc::ptr_eq(a, b))
                || new_named
                    .iter()
                    .zip(named_args.iter())
                    .any(|(a, b)| !Arc::ptr_eq(&a.node.value, &b.node.value));
            if changed {
                SurfaceExpression::Call {
                    func: new_func,
                    args: new_args,
                    named_args: new_named,
                    implied: *implied,
                }
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            let new_body = desugar_instance_decls_node(Arc::clone(body));
            if Arc::ptr_eq(&new_body, body) {
                expr.clone()
            } else {
                SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: new_body,
                    desugared: *desugared,
                }
            }
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            let new_scrutinee = desugar_instance_decls_node(Arc::clone(scrutinee));
            let new_arms: Vec<SurfaceMatchArm> = arms
                .iter()
                .map(|arm| {
                    let new_guard = arm
                        .guard
                        .as_ref()
                        .map(|g| desugar_instance_decls_node(Arc::clone(g)));
                    let new_body = desugar_instance_decls_node(Arc::clone(&arm.body));
                    SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: new_guard,
                        body: new_body,
                    }
                })
                .collect();
            let changed = !Arc::ptr_eq(&new_scrutinee, scrutinee)
                || new_arms.iter().zip(arms.iter()).any(|(a, b)| {
                    !Arc::ptr_eq(&a.body, &b.body)
                        || match (&a.guard, &b.guard) {
                            (Some(ag), Some(bg)) => !Arc::ptr_eq(ag, bg),
                            (None, None) => false,
                            _ => true,
                        }
                });
            if changed {
                SurfaceExpression::Match {
                    scrutinee: new_scrutinee,
                    arms: new_arms,
                }
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            let new_inner = desugar_instance_decls_node(Arc::clone(inner));
            if Arc::ptr_eq(&new_inner, inner) {
                expr.clone()
            } else {
                SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: new_inner,
                }
            }
        }
        // Leaf / non-recursive forms: return unchanged
        _ => expr.clone(),
    }
}

fn inject_adt_constructors_document(doc: &mut SurfaceDocument) {
    for item in &mut doc.items {
        if let SurfaceItem::Expr(node_arc) = item {
            let new_node = inject_adt_constructors_node(Arc::clone(node_arc));
            *node_arc = new_node;
        }
    }
}

fn inject_adt_constructors_node(node: Arc<SurfaceNode>) -> Arc<SurfaceNode> {
    let span = node.span.clone();
    let new_expr = inject_adt_constructors_expr(&node.expr, span.clone());
    if std::ptr::eq(&new_expr as *const _, &node.expr as *const _) {
        // No change — return original Arc (avoids clone)
        return node;
    }
    Arc::new(SurfaceNode {
        expr: new_expr,
        span,
    })
}

fn inject_adt_constructors_expr(expr: &SurfaceExpression, _span: Span) -> SurfaceExpression {
    match expr {
        SurfaceExpression::Dict(entries) => {
            let syn_span = Span::origin();
            let mut new_entries: Vec<Spanned<SurfaceEntry>> = Vec::new();
            let mut has_injection = false;
            // Track names that must not be injected — pre-seeded with any explicit (non-TypeAlias)
            // string key already present in this dict. This prevents E030 duplicate-key errors
            // when a [type ...] constructor name collides with an explicit binding.
            //
            // Example: the type-stage prelude has `Int: [TypeNode.Int]` (explicit) followed later
            // by `TypeNode: [type [Int@[...]] ...]` (TypeAlias). Without pre-seeding, inject_adt
            // would emit a second `Int:` entry, causing a duplicate-key runtime error.
            //
            // The set is also updated as constructors are injected, so two [type ...] declarations
            // in the same dict that share a constructor name don't produce duplicates either.
            let mut injected_names: std::collections::HashSet<String> = entries
                .iter()
                .filter_map(|e| {
                    // Only consider entries that are NOT declaration forms (TypeAlias, ClassDecl,
                    // InstanceDecl, MacroDecl). All Decl forms are skipped at runtime by lower.rs
                    // and produce no runtime binding, so they must not block constructor injection.
                    if matches!(&e.node.value.expr, SurfaceExpression::Decl(_)) {
                        return None;
                    }
                    // Extract the string key for non-Decl entries.
                    match e.node.key.as_ref().map(|k| &k.expr) {
                        Some(SurfaceExpression::Str(s)) => Some(s.clone()),
                        Some(SurfaceExpression::VarRef { name, .. }) => Some(name.clone()),
                        Some(SurfaceExpression::Annotated { name, .. }) => Some(name.clone()),
                        _ => None,
                    }
                })
                .collect();

            for se in entries {
                // Check if this entry's value is a TypeAlias Decl
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    if let SurfaceDeclaration::TypeAlias { body, .. } = decl.as_ref() {
                        // Extract type name from the dict entry key for tag qualification.
                        // Recognised key forms:
                        // - `Str(s)` — plain string key (resolver-phase name)
                        // - `VarRef { name }` — bare identifier key (pre-resolution name)
                        // - `Annotated { name, .. }` — annotated name key (T-1052):
                        //   `TypeName@[doc: "..." ...]` — the annotation is on the alias name.
                        //   The type name is extracted from the Annotated node; the annotation
                        //   itself is processed by typecheck.rs register_type_aliases.
                        // Computed keys are skipped (no injection for that entry).
                        // Absent keys (positional type declarations) use unqualified tags.
                        let type_name: Option<String> = match &se.node.key {
                            Some(key_node) => match &key_node.expr {
                                SurfaceExpression::Str(s) => Some(s.clone()),
                                SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                                // `TypeName@[doc: "..." ...]` — annotated alias name (T-1052).
                                // The annotation is on the alias declaration; name is the type name.
                                SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                                _ => {
                                    // Computed key — skip injection for this entry entirely.
                                    // (Proceed to add the original entry below.)
                                    None
                                }
                            },
                            None => {
                                // Positional type declaration — no type name available.
                                // Use unqualified tags (preserves backward compatibility).
                                None
                            }
                        };

                        // If key is computed (Some key_node but not Str/VarRef), we determined
                        // type_name = None above and we fall through to add the original entry.
                        // For Str/VarRef keys AND absent keys we still inject constructors.
                        let skip_injection = se.node.key.is_some() && type_name.is_none();

                        if !skip_injection {
                            let ctors = extract_surface_adt_ctors_from_expr(&body.expr);
                            if !ctors.is_empty() {
                                has_injection = true;
                                for ctor in ctors {
                                    if !injected_names.insert(ctor.name.clone()) {
                                        // Name already present (explicit binding or prior injection)
                                        // — skip to avoid E030 duplicate-key error.
                                        continue;
                                    }
                                    let qualified_tag = match &type_name {
                                        Some(tn) => format!("{}.{}", tn, ctor.name),
                                        None => ctor.name.clone(),
                                    };
                                    let key_node = Arc::new(SurfaceNode {
                                        expr: SurfaceExpression::Str(ctor.name.clone()),
                                        span: syn_span.clone(),
                                    });
                                    let value_node =
                                        build_constructor_value(&ctor, &qualified_tag, &syn_span);
                                    new_entries.push(Spanned::new(
                                        SurfaceEntry {
                                            key: Some(key_node),
                                            value: value_node,
                                        },
                                        syn_span.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
                // Include the original entry UNLESS a constructor with the same name as the
                // type key was already injected (which would create a duplicate key at runtime).
                // When CtorName == TypeKey, the injected constructor fn replaces the TypeAlias.
                let skip_original = if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    if let SurfaceDeclaration::TypeAlias { .. } = decl.as_ref() {
                        // Check if any injected constructor has the same name as this type key
                        if let Some(key_node) = &se.node.key {
                            match &key_node.expr {
                                SurfaceExpression::Str(s) => injected_names.contains(s),
                                SurfaceExpression::VarRef { name, .. } => {
                                    injected_names.contains(name)
                                }
                                // `TypeName@[...]` annotated alias key (T-1052).
                                SurfaceExpression::Annotated { name, .. } => {
                                    injected_names.contains(name)
                                }
                                _ => false,
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if skip_original {
                    // Constructor already injected with same key — TypeAlias would create duplicate
                    continue;
                }
                // Recurse into the entry's value to handle nested dicts
                let new_value = inject_adt_constructors_node(Arc::clone(&se.node.value));
                let new_key = se
                    .node
                    .key
                    .as_ref()
                    .map(|k| inject_adt_constructors_node(Arc::clone(k)));
                new_entries.push(Spanned::new(
                    SurfaceEntry {
                        key: new_key,
                        value: new_value,
                    },
                    se.span.clone(),
                ));
            }

            if has_injection {
                SurfaceExpression::Dict(new_entries)
            } else {
                // No injections: reconstruct only if children changed (propagate recursion)
                let changed = new_entries.iter().zip(entries.iter()).any(|(new, old)| {
                    !Arc::ptr_eq(&new.node.value, &old.node.value)
                        || new
                            .node
                            .key
                            .as_ref()
                            .zip(old.node.key.as_ref())
                            .is_some_and(|(a, b)| !Arc::ptr_eq(a, b))
                });
                if changed {
                    SurfaceExpression::Dict(new_entries)
                } else {
                    expr.clone()
                }
            }
        }
        // Recurse into other expression types that can contain dicts
        SurfaceExpression::Sequential(exprs) => {
            let new_exprs: Vec<Arc<SurfaceNode>> = exprs
                .iter()
                .map(|e| inject_adt_constructors_node(Arc::clone(e)))
                .collect();
            let changed = new_exprs
                .iter()
                .zip(exprs.iter())
                .any(|(a, b)| !Arc::ptr_eq(a, b));
            if changed {
                SurfaceExpression::Sequential(new_exprs)
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            let new_func = inject_adt_constructors_node(Arc::clone(func));
            let new_args: Vec<Arc<SurfaceNode>> = args
                .iter()
                .map(|a| inject_adt_constructors_node(Arc::clone(a)))
                .collect();
            let new_named: Vec<Spanned<SurfaceNamedArg>> = named_args
                .iter()
                .map(|na| {
                    let new_val = inject_adt_constructors_node(Arc::clone(&na.node.value));
                    Spanned::new(
                        SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: new_val,
                            annotation: na.node.annotation.clone(),
                        },
                        na.span.clone(),
                    )
                })
                .collect();
            let changed = !Arc::ptr_eq(&new_func, func)
                || new_args
                    .iter()
                    .zip(args.iter())
                    .any(|(a, b)| !Arc::ptr_eq(a, b))
                || new_named
                    .iter()
                    .zip(named_args.iter())
                    .any(|(a, b)| !Arc::ptr_eq(&a.node.value, &b.node.value));
            if changed {
                SurfaceExpression::Call {
                    func: new_func,
                    args: new_args,
                    named_args: new_named,
                    implied: *implied,
                }
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            let new_body = inject_adt_constructors_node(Arc::clone(body));
            if Arc::ptr_eq(&new_body, body) {
                expr.clone()
            } else {
                SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: new_body,
                    desugared: *desugared,
                }
            }
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            let new_scrutinee = inject_adt_constructors_node(Arc::clone(scrutinee));
            let new_arms: Vec<SurfaceMatchArm> = arms
                .iter()
                .map(|arm| {
                    let new_guard = arm
                        .guard
                        .as_ref()
                        .map(|g| inject_adt_constructors_node(Arc::clone(g)));
                    let new_body = inject_adt_constructors_node(Arc::clone(&arm.body));
                    SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: new_guard,
                        body: new_body,
                    }
                })
                .collect();
            let changed = !Arc::ptr_eq(&new_scrutinee, scrutinee)
                || new_arms.iter().zip(arms.iter()).any(|(a, b)| {
                    !Arc::ptr_eq(&a.body, &b.body)
                        || match (&a.guard, &b.guard) {
                            (Some(ag), Some(bg)) => !Arc::ptr_eq(ag, bg),
                            (None, None) => false,
                            _ => true,
                        }
                });
            if changed {
                SurfaceExpression::Match {
                    scrutinee: new_scrutinee,
                    arms: new_arms,
                }
            } else {
                expr.clone()
            }
        }
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            let new_inner = inject_adt_constructors_node(Arc::clone(inner));
            if Arc::ptr_eq(&new_inner, inner) {
                expr.clone()
            } else {
                SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: new_inner,
                }
            }
        }
        // Leaf / non-recursive forms: return unchanged
        _ => expr.clone(),
    }
}

/// The traversal role of an `@Child`-annotated field in a TypeNode constructor.
///
/// Derived from the field's declared type expression:
/// - `TypeNode`            → `One`       (single child, pass directly to `f`)
/// - `[Seq TypeNode]`      → `Seq`       (sequence of children, map `f` over elements)
/// - `[Map K TypeNode]`    → `MapValues` (map-keyed children, map `f` over values)
///
/// Intended to be stored in `field-annotations.{field}.role` inside `FnAnnotation.extra`
/// so that `child-fields`, `child-role`, and `child-field?` can read it generically via
/// `annotation-of` without per-constructor implementations. Population of the
/// `field-annotations:` dict from `@Child`-annotated fields is aspirational pending T-1124
/// (expression-valued annotation fields in `extract_fn_annotation_extra`).
///
/// Inferred from the field's declared type node: `TypeNode` → One, `[Seq TypeNode]` → Seq,
/// `[Map K TypeNode]` → MapValues. Used by `infer_child_role_from_type_expr` (T-1052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildRole {
    One,
    Seq,
    MapValues,
}

impl ChildRole {
    fn as_str(self) -> &'static str {
        match self {
            ChildRole::One => "One",
            ChildRole::Seq => "Seq",
            ChildRole::MapValues => "MapValues",
        }
    }
}

/// An `@Child`-annotated field within a named-field constructor.
///
/// `@Child` marks fields whose declared type contains `TypeNode` children — the traversal
/// role is inferred from the field type (`TypeNode` → One, `[Seq TypeNode]` → Seq,
/// `[Map K TypeNode]` → MapValues). Fields without `@Child` are non-children and pass
/// through unchanged in `map-children`.
///
/// Populated by `extract_surface_adt_ctors_from_expr` when a named arg carries a
/// `@Child` annotation (`SurfaceNamedArg.annotation = Some(Simple("Child"))`), added
/// by the parser in T-1052.
#[derive(Debug, Clone)]
struct ChildFieldAnnotation {
    /// The field name (e.g., `"types"`, `"fields"`, `"body"`).
    field_name: String,
    /// Traversal role inferred from the declared field type.
    role: ChildRole,
}

/// A constructor extracted from a TypeAlias body expression.
#[derive(Debug)]
struct AliasConstructor {
    /// Unqualified constructor name (e.g., `"Circle"`, `"Ok"`, `"None"`).
    name: String,
    /// Named field names, in order, for named-field constructors.
    /// Empty for unit constructors (bare VarRef, zero-arg Call, or positional-arg Call).
    fields: Vec<String>,
    /// Constructor-level annotation properties from `CtorName@[...]` syntax (T-1052).
    ///
    /// Each entry is a `(key, surface_node)` pair representing a key-value pair from the
    /// annotation PropertyDict, stored as `SurfaceExpression` nodes to be embedded in `return_ann`.
    /// Extracted from `SurfaceExpression::Annotated { annotation: PropertyDict([...]) }` by
    /// `extract_surface_adt_ctors_from_expr`.
    ctor_annotation_entries: Vec<(String, Arc<SurfaceNode>)>,
    /// Per-field `@Child` annotations for traversal protocol derivation (T-1052).
    ///
    /// Only fields explicitly annotated with `@Child` appear here. Fields without `@Child`
    /// are non-children and are not included in `field-annotations`.
    /// Extracted from `SurfaceNamedArg.annotation = Some(Simple("Child"))` or from Dict-frame
    /// keys `Annotated { name, annotation: Simple("Child") }`.
    child_fields: Vec<ChildFieldAnnotation>,
}

/// Infer the traversal role of an `@Child`-annotated field from its declared type expression.
///
/// Called from the T-1052 scaffolding in `extract_surface_adt_ctors_from_expr` once the
/// parser attaches `@Child` annotations to named args in constructor declarations.
///
/// The mapping is structural — it inspects the outermost Call node of the type expression:
///
/// - `TypeNode`                         → `ChildRole::One`       (bare name reference)
/// - `[Seq TypeNode]`                   → `ChildRole::Seq`       (Seq applied to TypeNode)
/// - `[Map K TypeNode]` (any K)         → `ChildRole::MapValues` (Map applied to key + TypeNode)
///
/// Any other type expression (including non-TypeNode arguments to Seq/Map) falls back to
/// `ChildRole::One` because the annotation presence already signals it is a child — the
/// role is the best conservative estimate from the structural shape.
///
/// Called from `extract_surface_adt_ctors_from_expr` when extracting `@Child`-annotated
/// fields from constructor declarations. `SurfaceNamedArg.annotation` (added in T-1052)
/// carries the `@Child` annotation; this function infers the traversal role from the value.
fn infer_child_role_from_type_expr(type_expr: &SurfaceExpression) -> ChildRole {
    match type_expr {
        // `[Seq TypeNode]` or `[Map K TypeNode]` — a Call node with an uppercase func name.
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied: true,
        } if named_args.is_empty() => {
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                match name.as_str() {
                    "Seq" if args.len() == 1 => {
                        // [Seq TypeNode] — Seq of children; role is Seq regardless of element type.
                        return ChildRole::Seq;
                    }
                    "Map" if args.len() == 2 => {
                        // [Map K TypeNode] — Map from keys to TypeNode children;
                        // role is MapValues (traverse values, preserve keys).
                        return ChildRole::MapValues;
                    }
                    _ => {}
                }
            }
            // Other call forms — conservatively treat as One.
            ChildRole::One
        }
        // Bare VarRef — `TypeNode` itself or any other simple type name → One.
        SurfaceExpression::VarRef { .. } => ChildRole::One,
        // Any other expression shape → One (conservative).
        _ => ChildRole::One,
    }
}

/// Extract all ADT constructors from a TypeAlias body expression.
///
/// Recognises two constructor forms:
///
/// 1. **Unit constructor** (no runtime fields):
///    - Bare uppercase `VarRef { name }` — e.g., `None`, `Red`, `Tcp`
///    - `Call { func: VarRef(UpperName), args: [...], named_args: [] }` — e.g., `[Ok a]`,
///      `[Some]`. Positional args are type-variable annotations and carry no runtime field
///      names, so these produce unit constructors.
///
/// 2. **Named-field constructor** (has runtime fields):
///    - `Call { func: VarRef(UpperName), named_args: [(name, _), ...] }` with at least one
///      named arg — e.g., `[Circle r: Int]`, `[Ok value: a]`. The field names (not types)
///      become field names recorded in `AliasConstructor.fields` for the variadic fn generator.
///
/// Entries with named dict keys (i.e., `se.node.key.is_some()`) are skipped — those are
/// record-body entries like `{value: Int next: Node}`, not constructor entries.
///
/// **Annotation extraction (T-1052 done, T-1053 done):** `ctor_annotation_entries` is
/// populated for `CtorName@[...]` syntax; `child_fields` for `field@Child: Type` syntax.
fn extract_surface_adt_ctors_from_expr(body: &SurfaceExpression) -> Vec<AliasConstructor> {
    let mut ctors = Vec::new();

    fn try_extract(expr: &SurfaceExpression, ctors: &mut Vec<AliasConstructor>) {
        /// Extract annotation entries from an `Annotation::PropertyDict` into `(key, value_node)` pairs.
        ///
        /// Filters out entries without a Str key (only Str-keyed entries are annotation fields).
        /// Returns an empty Vec for Simple and Annotated annotation forms — only PropertyDict
        /// carries constructor-level metadata key-value pairs.
        fn extract_ann_entries(ann: &Annotation) -> Vec<(String, Arc<SurfaceNode>)> {
            match ann {
                Annotation::PropertyDict(entries) => entries
                    .iter()
                    .filter_map(|e| {
                        let key_node = e.node.key.as_ref()?;
                        let key_str = match &key_node.expr {
                            SurfaceExpression::Str(s) => s.clone(),
                            _ => return None,
                        };
                        Some((key_str, Arc::clone(&e.node.value)))
                    })
                    .collect(),
                _ => Vec::new(),
            }
        }

        match expr {
            // Bare uppercase VarRef → unit constructor (no annotation)
            SurfaceExpression::VarRef { name, .. } if crate::eval::is_constructor_name(name) => {
                ctors.push(AliasConstructor {
                    name: name.clone(),
                    fields: Vec::new(),
                    ctor_annotation_entries: Vec::new(),
                    child_fields: Vec::new(),
                });
            }
            // Annotated uppercase name → unit constructor with constructor-level annotation.
            // `Union@[as-type: fn  guarding: false]` as a positional entry value in a
            // `[type ...]` body — the annotation carries type-level metadata for T-1053.
            SurfaceExpression::Annotated { name, annotation }
                if crate::eval::is_constructor_name(name) =>
            {
                let ctor_annotation_entries = extract_ann_entries(&annotation.node);
                ctors.push(AliasConstructor {
                    name: name.clone(),
                    fields: Vec::new(),
                    ctor_annotation_entries,
                    child_fields: Vec::new(),
                });
            }
            // Dict form: `[Constructor@[...] field: Type ...]` — opened as a Dict by Priority 2b
            // (Identifier + ImmediateAt in head position). The first positional entry is the
            // annotated constructor tag; keyed entries are named fields.
            //
            // Also handles the plain `[Constructor field: Type ...]` form (VarRef first entry).
            SurfaceExpression::Dict(entries) if !entries.is_empty() => {
                // First entry must be positional (no key) and be a VarRef or Annotated constructor.
                let first = &entries[0];
                if first.node.key.is_some() {
                    return; // No constructor tag in first position
                }
                let (ctor_name, ctor_annotation_entries) = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, .. }
                        if crate::eval::is_constructor_name(name) =>
                    {
                        (name.clone(), Vec::new())
                    }
                    SurfaceExpression::Annotated { name, annotation }
                        if crate::eval::is_constructor_name(name) =>
                    {
                        let entries = extract_ann_entries(&annotation.node);
                        (name.clone(), entries)
                    }
                    _ => return, // Not a constructor form
                };

                // Check whether any remaining entries are keyed (named fields).
                let keyed_entries: Vec<_> = entries[1..]
                    .iter()
                    .filter(|e| e.node.key.is_some())
                    .collect();
                if keyed_entries.is_empty() {
                    // No named fields — unit constructor
                    ctors.push(AliasConstructor {
                        name: ctor_name,
                        fields: Vec::new(),
                        ctor_annotation_entries,
                        child_fields: Vec::new(),
                    });
                } else {
                    // Named fields → named-field constructor.
                    // Collect field names for the variadic fn wrapper.
                    // For Dict-frame constructors, the key is either Str("field") or
                    // Annotated { name: "field", annotation: Simple("Child") }.
                    let fields: Vec<String> = keyed_entries
                        .iter()
                        .filter_map(|e| match e.node.key.as_ref().map(|k| &k.expr) {
                            Some(SurfaceExpression::Str(s)) => Some(s.clone()),
                            Some(SurfaceExpression::Annotated { name, .. }) => Some(name.clone()),
                            _ => None,
                        })
                        .collect();

                    // Populate child_fields from @Child-annotated field keys.
                    let child_fields: Vec<ChildFieldAnnotation> = keyed_entries
                        .iter()
                        .filter_map(|e| {
                            let key = e.node.key.as_ref()?;
                            if let SurfaceExpression::Annotated { name, annotation } = &key.expr {
                                // Check annotation is @Child (Simple("Child"))
                                if matches!(&annotation.node, Annotation::Simple(s) if s == "Child")
                                {
                                    let role = infer_child_role_from_type_expr(&e.node.value.expr);
                                    return Some(ChildFieldAnnotation {
                                        field_name: name.clone(),
                                        role,
                                    });
                                }
                            }
                            None
                        })
                        .collect();

                    ctors.push(AliasConstructor {
                        name: ctor_name,
                        fields,
                        ctor_annotation_entries,
                        child_fields,
                    });
                }
            }
            // Call with uppercase func → unit or named-field constructor
            // (for non-annotated brackets parsed as Call by Priority 3)
            SurfaceExpression::Call {
                func, named_args, ..
            } => {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if crate::eval::is_constructor_name(name) {
                        if named_args.is_empty() {
                            // Positional-only or zero-arg call → unit constructor
                            ctors.push(AliasConstructor {
                                name: name.clone(),
                                fields: Vec::new(),
                                ctor_annotation_entries: Vec::new(),
                                child_fields: Vec::new(),
                            });
                        } else {
                            // Named args → named-field constructor.
                            // Collect field names for the variadic fn wrapper.
                            let fields: Vec<String> =
                                named_args.iter().map(|na| na.node.name.clone()).collect();

                            // Populate child_fields from @Child-annotated named args (T-1052).
                            // `SurfaceNamedArg.annotation` carries the @Child annotation when the
                            // parser processes `field@Child: Type` syntax.
                            let child_fields: Vec<ChildFieldAnnotation> = named_args
                                .iter()
                                .filter_map(|na| {
                                    let annotation = na.node.annotation.as_ref()?;
                                    if !matches!(
                                        &annotation.node,
                                        Annotation::Simple(s) if s == "Child"
                                    ) {
                                        return None;
                                    }
                                    let role = infer_child_role_from_type_expr(&na.node.value.expr);
                                    Some(ChildFieldAnnotation {
                                        field_name: na.node.name.clone(),
                                        role,
                                    })
                                })
                                .collect();

                            ctors.push(AliasConstructor {
                                name: name.clone(),
                                fields,
                                ctor_annotation_entries: Vec::new(),
                                child_fields,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    match body {
        SurfaceExpression::Dict(entries) => {
            // Detect the "annotated constructor with named fields" pattern:
            // `[Union@[...] types: [Seq TypeNode]]` — first positional entry is a VarRef or
            // Annotated constructor tag, at least one subsequent entry is keyed (named field).
            // This is a single named-field constructor declaration, not a union of constructors.
            // Treat the whole Dict as one constructor (via try_extract's Dict arm).
            //
            // vs. "union of constructors" pattern:
            // `[[Ok a] [Err String]]` — all entries are positional; each is a constructor dict.
            //
            // Disambiguation: if the first positional entry value is a VarRef/Annotated constructor
            // AND at least one of the remaining entries has a key → it's a constructor dict.
            // Otherwise → it's a union (process each positional entry individually).
            let is_constructor_dict = if let Some(first) = entries.first() {
                if first.node.key.is_none() {
                    let first_is_ctor = matches!(
                        &first.node.value.expr,
                        SurfaceExpression::VarRef { name, .. }
                            if crate::eval::is_constructor_name(name)
                    ) || matches!(
                        &first.node.value.expr,
                        SurfaceExpression::Annotated { name, .. }
                            if crate::eval::is_constructor_name(name)
                    );
                    let has_keyed_entries = entries[1..].iter().any(|e| e.node.key.is_some());
                    first_is_ctor && has_keyed_entries
                } else {
                    false
                }
            } else {
                false
            };

            if is_constructor_dict {
                // Single constructor dict: treat the whole Dict as one constructor declaration.
                try_extract(body, &mut ctors);
            } else {
                // Union of constructors: each positional entry is a separate constructor.
                for entry in entries {
                    if entry.node.key.is_none() {
                        // Positional entry in [type ...] body — may be a constructor
                        try_extract(&entry.node.value.expr, &mut ctors);
                    }
                    // Named-key entries are record-body type annotations, not constructors — skip.
                }
            }
        }
        other => {
            // Single-entry type body (no dict wrapper)
            try_extract(other, &mut ctors);
        }
    }

    ctors
}

/// Build the annotation entries to store in the constructor function's `return_ann`.
///
/// The `return_ann` of the injected `SurfaceExpression::Fn` carries the constructor's
/// annotation data as a `PropertyDict`. When T-1049 lands, the evaluator will populate
/// `FnAnnotation.extra` from the non-standard fields of this PropertyDict at function
/// evaluation time.
///
/// **Layout of the PropertyDict entries:**
///
/// 1. **Constructor-level annotation fields** — from `CtorName@[key: val ...]` syntax
///    (T-1052). These are plain key-value entries, e.g. `as-type: [fn ...]`, `guarding: false`.
///    Copied verbatim from `ctor.ctor_annotation_entries`.
///
/// 2. **`field-annotations:` entry** — present only when at least one `@Child` field exists.
///    A synthetic dict mapping each `@Child` field name to its annotation sub-dict:
///    ```
///    [field-name-1: [role: "Seq"] field-name-2: [role: "One"] ...]
///    ```
///    Consumed by `child-fields`, `child-role`, and `child-field?` helper functions via
///    `annotation-of` — enabling the traversal protocol to be derived generically without
///    per-constructor implementations.
///
/// Returns `None` when there is nothing to encode (no constructor-level annotation entries
/// and no `@Child` fields). The evaluator treats `return_ann: None` as "no annotation",
/// which is correct — the `FnAnnotation.extra` map will be empty by default (T-1049).
///
/// With T-1052 landed, `ctor.ctor_annotation_entries` is populated from `CtorName@[...]`
/// syntax and `ctor.child_fields` is populated from `field@Child: Type` syntax. This
/// function now produces non-trivial PropertyDict annotations for annotated constructors.
fn build_constructor_return_ann(
    ctor: &AliasConstructor,
    syn_span: &Span,
) -> Option<Spanned<Annotation>> {
    // Collect all entries for the PropertyDict annotation.
    let mut entries: Vec<Spanned<SurfaceEntry>> = Vec::new();

    // 1. Constructor-level annotation fields (from CtorName@[...] syntax, T-1052).
    for (key, value_node) in &ctor.ctor_annotation_entries {
        let key_node = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Str(key.clone()),
            span: syn_span.clone(),
        });
        entries.push(Spanned::new(
            SurfaceEntry {
                key: Some(key_node),
                value: Arc::clone(value_node),
            },
            syn_span.clone(),
        ));
    }

    // 2. `field-annotations:` entry for @Child fields (from field@Child: Type syntax, T-1052).
    //    Only included when at least one @Child field was found.
    if !ctor.child_fields.is_empty() {
        // Build the inner dict: [field-name-1: [role: "Seq"] field-name-2: [role: "One"] ...]
        let field_ann_entries: Vec<Spanned<SurfaceEntry>> = ctor
            .child_fields
            .iter()
            .map(|cf| {
                // Build [role: "Seq"|"One"|"MapValues"] — the per-field annotation sub-dict.
                let role_key = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Str("role".to_string()),
                    span: syn_span.clone(),
                });
                let role_val = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Str(cf.role.as_str().to_string()),
                    span: syn_span.clone(),
                });
                let role_entry = Spanned::new(
                    SurfaceEntry {
                        key: Some(role_key),
                        value: role_val,
                    },
                    syn_span.clone(),
                );
                let field_ann_dict = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Dict(vec![role_entry]),
                    span: syn_span.clone(),
                });

                // Build the outer entry: field-name: [role: "..."]
                let field_key = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Str(cf.field_name.clone()),
                    span: syn_span.clone(),
                });
                Spanned::new(
                    SurfaceEntry {
                        key: Some(field_key),
                        value: field_ann_dict,
                    },
                    syn_span.clone(),
                )
            })
            .collect();

        // Build the `field-annotations` value: [field1: [role: "Seq"] field2: [role: "One"] ...]
        let field_annotations_dict = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Dict(field_ann_entries),
            span: syn_span.clone(),
        });

        // Add the `field-annotations:` key to the outer PropertyDict.
        let fa_key = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Str("field-annotations".to_string()),
            span: syn_span.clone(),
        });
        entries.push(Spanned::new(
            SurfaceEntry {
                key: Some(fa_key),
                value: field_annotations_dict,
            },
            syn_span.clone(),
        ));
    }

    if entries.is_empty() {
        // No annotation data — leave return_ann as None.
        return None;
    }

    Some(Spanned::new(
        Annotation::PropertyDict(entries),
        syn_span.clone(),
    ))
}

/// Build the SurfaceExpression value for an injected constructor entry.
///
/// - Unit constructor (`ctor.fields` is empty):
///   - Unannotated: `[variant "TypeName.CtorName"]`
///   - Annotated (`CtorName@[key: val ...]`):
///     `[make-annotated [variant "TypeName.CtorName"] [key: val ...]]`
///     The annotation PropertyDict entries are converted to a plain dict literal. At eval time,
///     `make-annotated` (a prelude alias of `builtin-make-annotated`) materializes both the
///     variant and the annotation dict and returns
///     `Value::Annotated { inner: Variant(...), annotation: Dict({...}) }`.
///
/// - Named-field constructor (`ctor.fields` non-empty):
///   `[fn@[...] [...payload] [variant-payload "TypeName.CtorName" payload]]`
///   A variadic `...payload` param collects all named args into a dict at runtime
///   (via bind_args_thunks B-277 / C-NAMED-VALID amended for variadics).
///   Callers use named args: `[Ctor field1: val1 field2: val2]`.
///
///   Why variadic instead of per-field named params:
///   Using `[fn [let field1 field2 ...] [variant-payload "Tag" [field1: field1 ...]]]`
///   causes a letrec self-reference. The inner payload dict `[field1: field1 ...]` enters
///   a letrec scope that shadows the fn params with the SAME names. The resolver assigns
///   level=0 (inner dict scope) to the VarRef references, making each dict value reference
///   ITS OWN entry rather than the fn param — cycle error at runtime.
///
///   The variadic form avoids this: `payload` is collected by bind_args_thunks into a
///   materialized Dict BEFORE any inner dict is constructed in the body. The body
///   `[variant-payload "Tag" payload]` simply references the already-bound `payload` param.
///
/// **Annotation encoding:**
/// - Unit constructors (T-1121): `build_constructor_return_ann` produces `Some(PropertyDict)`
///   when the constructor is annotated. The PropertyDict entries are emitted as a plain dict
///   literal argument to `make-annotated`, which wraps the result in `Value::Annotated`.
/// - Named-field constructors (T-1053): the annotation is encoded in `return_ann` on the Fn node.
///   The evaluator reads `return_ann` via `extract_fn_annotation_extra` and populates
///   `FnAnnotation.extra`, making annotations available via `annotation-of` on the constructor.
fn build_constructor_value(
    ctor: &AliasConstructor,
    qualified_tag: &str,
    syn_span: &Span,
) -> Arc<SurfaceNode> {
    // Build the annotation for the constructor Fn node (if any).
    // This encodes constructor-level @[...] fields and field-annotations: from @Child fields.
    // Returns None for unannotated constructors; Some(PropertyDict) when the constructor
    // carries @[...] annotation entries or @Child field annotations (both require T-1052, done).
    let constructor_return_ann = build_constructor_return_ann(ctor, syn_span);

    // For unit constructors: use the prelude's 1-arg `variant` wrapper.
    // For named-field constructors: use `variant-payload` (the 2-arg wrapper).
    // Both delegate to `builtin-variant`; split because the prelude's `variant`
    // only accepts 1 arg.
    let unit_variant_fn = Arc::new(SurfaceNode {
        expr: SurfaceExpression::VarRef {
            name: "variant".to_string(),
            escaped: false,
        },
        span: syn_span.clone(),
    });
    let payload_variant_fn = Arc::new(SurfaceNode {
        expr: SurfaceExpression::VarRef {
            name: "variant-payload".to_string(),
            escaped: false,
        },
        span: syn_span.clone(),
    });
    let tag_arg = Arc::new(SurfaceNode {
        expr: SurfaceExpression::Str(qualified_tag.to_string()),
        span: syn_span.clone(),
    });

    if ctor.fields.is_empty() {
        // Unit constructor: [variant "TypeName.CtorName"]
        //
        // T-1121: When the constructor carries a @[...] annotation (constructor_return_ann is
        // Some(Annotation::PropertyDict(entries))), wrap the unit variant in Value::Annotated
        // by emitting:
        //   [make-annotated [variant "TypeName.CtorName"] [key: val ...]]
        //
        // The PropertyDict entries are converted directly to a SurfaceExpression::Dict node,
        // so the annotation dict is evaluated at the same time as the constructor value.
        // Both arguments are materialized by make-annotated's [Seq, Seq] strictness.
        //
        // When there is no annotation (constructor_return_ann is None), emit the plain variant
        // call as before.

        // Build [variant "TypeName.CtorName"]
        let variant_call = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Call {
                func: unit_variant_fn,
                args: vec![tag_arg],
                named_args: vec![],
                implied: false,
            },
            span: syn_span.clone(),
        });

        // If there are annotation entries, wrap in [make-annotated variant-call ann-dict]
        match constructor_return_ann {
            Some(ann_spanned) => {
                // Extract the PropertyDict entries from the annotation.
                // build_constructor_return_ann always produces Annotation::PropertyDict when Some.
                let Annotation::PropertyDict(ann_entries) = ann_spanned.node else {
                    // Unexpected annotation form (not PropertyDict) — emit unannotated variant.
                    // This branch is unreachable in practice: build_constructor_return_ann only
                    // returns Some(PropertyDict). Guard here to avoid a panic.
                    return variant_call;
                };

                // Convert the PropertyDict entries to a plain SurfaceExpression::Dict.
                // The entries are already SurfaceEntry values with Str keys and expression values.
                let ann_dict_node = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Dict(ann_entries),
                    span: syn_span.clone(),
                });

                // Build the make-annotated function reference (prelude wrapper of builtin-make-annotated).
                // Consistent with using "variant" (not "builtin-variant") for unit variant calls.
                let make_annotated_fn = Arc::new(SurfaceNode {
                    expr: SurfaceExpression::VarRef {
                        name: "make-annotated".to_string(),
                        escaped: false,
                    },
                    span: syn_span.clone(),
                });

                // Emit [make-annotated [variant "Tag"] [key: val ...]]
                Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Call {
                        func: make_annotated_fn,
                        args: vec![variant_call, ann_dict_node],
                        named_args: vec![],
                        implied: false,
                    },
                    span: syn_span.clone(),
                })
            }
            None => variant_call,
        }
    } else {
        // Named-field constructor: variadic fn that collects named args into a payload dict.
        //
        // Calling convention: the variadic `...payload` param collects all named args (e.g.,
        // `field1: val1 field2: val2`) into a single Dict at runtime, via the B-277 extension
        // to bind_args_thunks (C-NAMED-VALID amended: unmatched named args flow into variadic).
        //
        // Generates: [fn@[...] [...payload] [variant-payload "TypeName.CtorName" payload]]
        // Callers use: `[Ctor field1: val1 field2: val2]`
        //
        // Uses `variant-payload` (the prelude's 2-arg wrapper around builtin-variant) because
        // the prelude's `variant` only accepts 1 arg (tag only, no payload). The 2-arg form
        // requires `variant-payload`.

        // Build [variant-payload "TypeName.CtorName" payload] — body references the variadic param
        let payload_ref = Arc::new(SurfaceNode {
            expr: SurfaceExpression::VarRef {
                name: "payload".to_string(),
                escaped: false,
            },
            span: syn_span.clone(),
        });

        // Build [variant-payload "TypeName.CtorName" payload]
        let variant_call = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Call {
                func: payload_variant_fn,
                args: vec![tag_arg, payload_ref],
                named_args: vec![],
                implied: false,
            },
            span: syn_span.clone(),
        });

        // Build fn params: [...payload] — single variadic param collecting all named args
        let params: Vec<Spanned<SurfaceParam>> = vec![Spanned::new(
            SurfaceParam {
                name: "payload".to_string(),
                annotation: None,
                variadic: true,
            },
            syn_span.clone(),
        )];

        // Build [fn@[...] [...payload] [variant-payload "TypeName.CtorName" payload]]
        // return_ann carries the constructor annotation (constructor-level @[...] fields +
        // field-annotations: from @Child). The evaluator reads return_ann via
        // extract_fn_annotation_extra and populates FnAnnotation.extra (T-1049 done).
        Arc::new(SurfaceNode {
            expr: SurfaceExpression::Fn {
                return_ann: constructor_return_ann,
                params,
                body: variant_call,
                desugared: true, // Synthetic: suppress $_ auto-lambda wrapping
            },
            span: syn_span.clone(),
        })
    }
}

/// Desugar a single SurfaceDocument (all expression items).
fn desugar_surface_document(doc: &mut SurfaceDocument) {
    for item in &mut doc.items {
        if let SurfaceItem::Expr(node_arc) = item {
            desugar_surface_node(node_arc, 0);
        }
        // Skip SurfaceItem::Decl — declarations are handled by the expander, not the evaluator
    }
}

/// Desugar a single SurfaceNode at the given lexical depth.
///
/// `depth` tracks how many enclosing `Fn([_] ...)` lambdas we are inside:
/// - `depth = 0`: `$_` is unbound, WRAP rules apply
/// - `depth > 0`: `$_` is bound by an enclosing lambda, only recurse (no wrapping)
///
/// This is the stable public entry point for callers that hold a standalone
/// `Arc<SurfaceNode>` (e.g., REPL input, eval.rs test helpers). The private
/// `desugar_surface` function is the recursive implementation; this wrapper
/// keeps the public API surface stable while allowing internal refactors.
///
/// Mutates the Arc<SurfaceNode> in place using Arc::make_mut.
pub fn desugar_surface_node(node: &mut Arc<SurfaceNode>, depth: usize) {
    desugar_surface(node, depth);
}

/// Core Surface desugaring logic: top-down traversal with selective wrapping.
///
/// At depth=0, check WRAP conditions on raw children BEFORE recursing.
/// If any child is DIRECT, wrap the whole expression in `[fn [_] ...]`.
/// Then recurse into children at depth+1 (inside the lambda, `$_` is bound).
///
/// At depth>0, only recurse into children (no wrapping).
fn desugar_surface(node: &mut Arc<SurfaceNode>, depth: usize) {
    // At depth 0, try to wrap based on raw children
    if depth == 0 && try_wrap_surface(node) {
        // After wrapping, the body is at depth+1 (inside the generated lambda)
        // We need to recurse into the wrapped body
        let node_mut = Arc::make_mut(node);
        if let SurfaceExpression::Fn { body, .. } = &mut node_mut.expr {
            desugar_surface(body, 1);
        }
        return;
    }

    // No wrapping occurred (or depth > 0): recurse into children
    recurse_children_surface(node, depth);
}

/// Check if this SurfaceNode should be wrapped based on DIRECT children.
///
/// Returns `true` if wrapping occurred, `false` otherwise.
/// Mutates `node` in place by replacing it with `[fn [_] original_expr]`.
fn try_wrap_surface(node: &mut Arc<SurfaceNode>) -> bool {
    match &node.expr {
        // WRAP-CALL: any arg (not func position) is DIRECT
        SurfaceExpression::Call {
            args, named_args, ..
        } => {
            // Func position excluded from WRAP check
            let has_direct_arg = args.iter().any(|a| is_direct_underscore_surface(&a.expr))
                || named_args
                    .iter()
                    .any(|na| is_direct_underscore_surface(&na.node.value.expr));

            if has_direct_arg {
                wrap_surface_in_lambda(node);
                return true;
            }
            false
        }

        // WRAP-DICT: any value (not key) is DIRECT
        SurfaceExpression::Dict(entries) => {
            let has_direct_value = entries
                .iter()
                .any(|e| is_direct_underscore_surface(&e.node.value.expr));

            if has_direct_value {
                wrap_surface_in_lambda(node);
                return true;
            }
            false
        }

        // WRAP-DOT: target is DIRECT (single $_ or access chain on $_)
        SurfaceExpression::DotAccess { expr: target, .. } => {
            if is_direct_underscore_surface(&target.expr) {
                wrap_surface_in_lambda(node);
                return true;
            }
            false
        }

        // WRAP-PIPE: LHS is DIRECT (e.g., `$_ | f` becomes `[fn [_] $_ | f]`)
        SurfaceExpression::Pipe { lhs, .. } => {
            if is_direct_underscore_surface(&lhs.expr) {
                wrap_surface_in_lambda(node);
                return true;
            }
            false
        }

        // All other cases: no wrapping
        _ => false,
    }
}

/// DIRECT predicate: tests whether a SurfaceExpression is `$_` or an access chain rooted at `$_`.
fn is_direct_underscore_surface(expr: &SurfaceExpression) -> bool {
    match expr {
        SurfaceExpression::VarRef { name, .. } => name == "_",
        // Access chains on $_ count as DIRECT (e.g., $_.name)
        SurfaceExpression::DotAccess { expr: inner, .. } => {
            is_direct_underscore_surface(&inner.expr)
        }
        // Pipe chains: check LHS (e.g., $_ | f becomes [fn [_] $_ | f])
        SurfaceExpression::Pipe { lhs, .. } => is_direct_underscore_surface(&lhs.expr),
        // All other expressions: not DIRECT
        _ => false,
    }
}

/// Wrap a SurfaceNode in `[fn [_] original_expr]`.
///
/// Mutates `node` in place. The generated Fn node inherits the outer expression's span,
/// preserving the original inner expression's span structure for the body.
fn wrap_surface_in_lambda(node: &mut Arc<SurfaceNode>) {
    let span = node.span.clone();
    // Clone the Arc to preserve the original node as the body
    let body = Arc::clone(node);

    *node = Arc::new(SurfaceNode {
        expr: SurfaceExpression::Fn {
            return_ann: None,
            params: vec![Spanned::new(
                SurfaceParam {
                    name: "_".to_string(),
                    annotation: None,
                    variadic: false,
                },
                span.clone(),
            )],
            body,
            desugared: true,
        },
        span,
    });
}

/// Recurse into all children of a SurfaceNode at the given depth.
///
/// For `Fn` nodes with `_` parameter, increment depth when recursing into the body
/// to suppress WRAP at depth > 0 (shadowing).
fn recurse_children_surface(node: &mut Arc<SurfaceNode>, depth: usize) {
    let node_mut = Arc::make_mut(node);

    match &mut node_mut.expr {
        // Literals: no children
        SurfaceExpression::Int(_)
        | SurfaceExpression::U64(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Decl(_) // type-level declaration, no evaluable children
        | SurfaceExpression::Error(_) => {}

        // Access expressions: recurse into target
        SurfaceExpression::DotAccess { expr: target, .. } => {
            desugar_surface(target, depth);
        }

        // Pipe: collect the right-associative chain into a flat list of stages, desugar each
        // stage independently, then left-fold into nested calls.
        //
        // The parser produces right-associative trees: `a | b | c | d` parses as
        // `Pipe(a, Pipe(b, Pipe(c, d)))`. A naïve recurse-then-rewrite approach would first
        // desugar the inner `Pipe(b, Pipe(c, d))` into `Call(d, [c, b])`, and then the outer
        // `desugar_pipe_surface` would see a `Call` rhs and append `a`, producing `[d c b a]`
        // (flat) instead of `[d [c [b a]]]` (correctly nested left-folded calls).
        //
        // The correct fix: flatten the chain into stages [a, b, c, d], desugar each stage
        // independently (not as a pipe sub-chain), then left-fold:
        //   acc = a; acc = [b acc]; acc = [c acc]; acc = [d acc]
        // producing the correct `[d [c [b a]]]`.
        SurfaceExpression::Pipe { .. } => {
            desugar_pipe_chain(node, depth);
        }

        // Sequential: recurse into all expressions
        SurfaceExpression::Sequential(exprs) => {
            for seq_expr in exprs {
                desugar_surface(seq_expr, depth);
            }
        }

        // Dict: recurse into keys and values
        SurfaceExpression::Dict(entries) => {
            for entry_spanned in entries {
                desugar_surface_entry(&mut entry_spanned.node, depth);
            }
        }

        // Call: recurse into func, args, and named args
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            desugar_surface(func, depth);
            for arg in args {
                desugar_surface(arg, depth);
            }
            for named_arg_spanned in named_args {
                desugar_surface(&mut named_arg_spanned.node.value, depth);
            }
        }

        // Fn: increment depth if `_` is a parameter, then recurse into body
        SurfaceExpression::Fn {
            params,
            body,
            return_ann,
            ..
        } => {
            // Check if `_` is a parameter (shadowing)
            let has_underscore_param = params.iter().any(|p| p.node.name == "_");
            let new_depth = if has_underscore_param {
                depth + 1
            } else {
                depth
            };

            // Recurse into parameter annotations (if they contain expressions)
            for param_spanned in params {
                desugar_surface_annotation_option(&mut param_spanned.node.annotation, depth);
            }

            // Recurse into return annotation (if it contains expressions)
            desugar_surface_annotation_option(return_ann, depth);

            // Recurse into body at new depth
            desugar_surface(body, new_depth);
        }

        // TypeAssert: recurse into annotation and expression
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            desugar_surface_annotation(&mut annotation.node, depth);
            desugar_surface(inner, depth);
        }

        // Annotated: recurse into annotation
        SurfaceExpression::Annotated { annotation, .. } => {
            desugar_surface_annotation(&mut annotation.node, depth);
        }

        // Quote: DO NOT recurse into the quoted expression.
        // $_ inside a quote should remain as-is (AST frozen).
        SurfaceExpression::Quote(_) => {}

        // Unquote and UnquoteSplice: DO recurse into the unquoted expression.
        SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
            desugar_surface(inner, depth);
        }

        // Match: recurse into scrutinee and arm bodies (but not patterns).
        SurfaceExpression::Match { scrutinee, arms } => {
            desugar_surface(scrutinee, depth);
            for arm in arms {
                desugar_surface(&mut arm.body, depth);
            }
        }

        // Binding and pattern forms: recurse into child expressions
        SurfaceExpression::PatternDecl { bindings } => {
            for binding in bindings {
                desugar_surface(binding, depth);
            }
        }
        SurfaceExpression::LetDecl { bindings } => {
            for binding in bindings {
                desugar_surface(binding, depth);
            }
        }
        SurfaceExpression::CaseArm { pattern, body, .. } => {
            desugar_surface(pattern, depth);
            desugar_surface(body, depth);
        }
    }
}

/// Desugar a SurfaceEntry (key and value).
fn desugar_surface_entry(entry: &mut SurfaceEntry, depth: usize) {
    if let Some(key_arc) = &mut entry.key {
        desugar_surface(key_arc, depth);
    }
    desugar_surface(&mut entry.value, depth);
}

/// Desugar an annotation (if it's a PropertyDict with expression values).
#[allow(clippy::only_used_in_recursion)] // depth is passed through recursive calls for future use
fn desugar_surface_annotation(ann: &mut Annotation, depth: usize) {
    match ann {
        Annotation::Simple(_) => {}
        Annotation::PropertyDict(_entries) => {
            // PropertyDict entries use the old Entry/Expr AST types. $_ inside type
            // annotations is a user error (type expressions don't evaluate), so we
            // do not recurse here.
        }
        Annotation::Annotated(_name, inner) => {
            desugar_surface_annotation(inner, depth);
        }
    }
}

/// Desugar a pipe chain starting at `node` (which must be a `Pipe` node).
///
/// Flattens the right-associative chain into stages, desugars each stage independently,
/// then left-folds them into nested `Call` nodes using `apply_pipe_step`.
///
/// For `a | b | c | d` (parsed as `Pipe(a, Pipe(b, Pipe(c, d)))`):
/// - Stages: `[a, b, c, d]`
/// - After fold: `apply_pipe_step(apply_pipe_step(apply_pipe_step(a, b), c), d)`
/// - Result: `[d [c [b a]]]` (correct left-associative nesting)
fn desugar_pipe_chain(node: &mut Arc<SurfaceNode>, depth: usize) {
    // Collect all pipe stages by walking the right-associative chain.
    // The span of the outermost Pipe node is used for the final result node.
    let span = node.span.clone();
    let mut stages: Vec<Arc<SurfaceNode>> = Vec::new();
    collect_pipe_stages(node, &mut stages);

    // Desugar each stage independently (not as part of a pipe chain).
    for stage in &mut stages {
        desugar_surface(stage, depth);
    }

    // Left-fold the stages: acc = stages[0], then for each subsequent stage,
    // acc = apply_pipe_step(acc, stage).
    debug_assert!(stages.len() >= 2, "Pipe node must have at least two stages");
    let mut stages_iter = stages.into_iter();
    let mut acc: Arc<SurfaceNode> = stages_iter.next().expect("at least one stage");
    for step in stages_iter {
        acc = apply_pipe_step(acc, step, span.clone());
    }

    // Replace node's expression with the folded result.
    Arc::make_mut(node).expr = acc.expr.clone();
}

/// Collect all stages of a right-associative pipe chain into a flat `Vec`.
///
/// `Pipe(a, Pipe(b, Pipe(c, d)))` → `[a, b, c, d]`.
///
/// Only `Pipe` nodes are unwrapped; any non-`Pipe` node becomes a leaf stage.
fn collect_pipe_stages(node: &Arc<SurfaceNode>, stages: &mut Vec<Arc<SurfaceNode>>) {
    match &node.expr {
        SurfaceExpression::Pipe { lhs, rhs } => {
            stages.push(Arc::clone(lhs));
            collect_pipe_stages(rhs, stages);
        }
        _ => {
            stages.push(Arc::clone(node));
        }
    }
}

/// Apply one pipe step: `lhs | rhs_stage` → `Call`.
///
/// Rules (applied to the already-desugared `rhs_stage`):
/// - `Pipe(lhs, Call(f, args))` → `Call(f, args ++ [lhs])`  (extend existing call)
/// - `Pipe(lhs, VarRef(n))`    → `Call(VarRef(n), [lhs])`
/// - `Pipe(lhs, other)`        → `Call(other, [lhs])`
///
/// Note: `rhs_stage` must already be desugared and must NOT be a `Pipe` node
/// (all `Pipe` nodes in the chain were collected and desugared before folding).
fn apply_pipe_step(
    lhs: Arc<SurfaceNode>,
    rhs_stage: Arc<SurfaceNode>,
    span: Span,
) -> Arc<SurfaceNode> {
    let new_expr = match &rhs_stage.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // rhs_stage is an explicit call in the source (e.g., `[f x y]`).
            // Append lhs as the final positional argument.
            let mut new_args = args.clone();
            new_args.push(lhs);
            SurfaceExpression::Call {
                func: Arc::clone(func),
                args: new_args,
                named_args: named_args.clone(),
                implied: *implied,
            }
        }
        SurfaceExpression::VarRef { name, escaped } => {
            // Bare name (e.g., `a | f`): call it with lhs as the sole argument.
            SurfaceExpression::Call {
                func: Arc::new(SurfaceNode {
                    expr: SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped: *escaped,
                    },
                    span: rhs_stage.span.clone(),
                }),
                args: vec![lhs],
                named_args: vec![],
                implied: true,
            }
        }
        _ => {
            // Any other desugared expression: call it with lhs.
            SurfaceExpression::Call {
                func: rhs_stage,
                args: vec![lhs],
                named_args: vec![],
                implied: true,
            }
        }
    };

    Arc::new(SurfaceNode {
        expr: new_expr,
        span,
    })
}

/// Desugar an optional annotation.
fn desugar_surface_annotation_option(ann: &mut Option<Spanned<Annotation>>, depth: usize) {
    if let Some(ann_spanned) = ann {
        desugar_surface_annotation(&mut ann_spanned.node, depth);
    }
}
