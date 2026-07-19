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
        let count = Arc::strong_count(&doc_spanned.node);
        desugar_surface_document(
            Arc::get_mut(&mut doc_spanned.node).unwrap_or_else(|| {
                panic!("document Arc has {count} strong references, expected 1")
            }),
        );
    }
}

/// Transform single-arm InstanceDecl in dict-entry position into runtime method dicts (T-1142,
/// B-409).
///
/// For each dict entry whose value is a `SurfaceExpression::Decl(InstanceDecl)` with exactly
/// one arm, this pass extracts the instance methods from that arm and replaces the Decl with an
/// explicit `SurfaceExpression::Dict` containing those methods. This enables runtime method access
/// (e.g., `MonadResult.bind`) without special-casing InstanceDecl in the lowering pass.
///
/// **Single-arm only:** only fires for named single-arm instances in dict-entry position, e.g.:
///   `MonadResult: [instance Monad [let m@Result]: [bind: ...]]`
///
/// Multi-arm instances (e.g., `Addable` with several `[let a@T b@U c]` arms) are left as
/// `SurfaceExpression::Decl(InstanceDecl)` so lower.rs can emit all arms as
/// instance-binding-name-keyed dict entries (via `instance_binding_name`). Transforming only
/// `arms[0]` would silently discard all subsequent arms.
///
/// Runs BEFORE `desugar_surface_program` (`$_` desugaring and pipe lowering).
pub fn desugar_instance_decls_surface_program(program: &mut SurfaceProgram) {
    for doc_spanned in &mut program.documents {
        desugar_instance_decls_document(
            Arc::get_mut(&mut doc_spanned.node).expect("desugar runs before any Arc sharing"),
        );
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
    Arc::new(SurfaceNode::new(new_expr, span))
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
                        if arms.len() == 1 {
                            // Single-arm instance: extract methods from the one arm and build a
                            // plain Dict. Multi-arm instances (arms.len() > 1) are left as Decl
                            // so lower.rs processes all arms via instance_binding_name (B-409).
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
                            Some(Arc::new(SurfaceNode::new(
                                SurfaceExpression::Dict(desugared_methods),
                                se.node.value.span.clone(),
                            )))
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
                    let new_body = arm
                        .body
                        .iter()
                        .map(|e| desugar_instance_decls_node(Arc::clone(e)))
                        .collect();
                    SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: new_guard,
                        body: new_body,
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                    }
                })
                .collect();
            let changed = !Arc::ptr_eq(&new_scrutinee, scrutinee)
                || new_arms.iter().zip(arms.iter()).any(|(a, b)| {
                    (a.body.len() != b.body.len()
                        || a.body
                            .iter()
                            .zip(b.body.iter())
                            .any(|(x, y)| !Arc::ptr_eq(x, y)))
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
            ..
        } => {
            let new_inner = desugar_instance_decls_node(Arc::clone(inner));
            if Arc::ptr_eq(&new_inner, inner) {
                expr.clone()
            } else {
                SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: new_inner,
                    resolved_type: crate::ast::TypeAnnotation::new(),
                }
            }
        }
        // Leaf / non-recursive forms: return unchanged
        _ => expr.clone(),
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

        // WRAP-DOT: target is DIRECT (single $_ or access chain on $_).
        // Leading-dot (expr: None) is never a $_ chain — it references a parent-scope name.
        SurfaceExpression::Field {
            expr: Some(target), ..
        } => {
            if is_direct_underscore_surface(&target.expr) {
                wrap_surface_in_lambda(node);
                return true;
            }
            false
        }
        SurfaceExpression::Field { expr: None, .. } => false,

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
        // Access chains on $_ count as DIRECT (e.g., $_.name).
        // Leading-dot (expr: None) is a parent-scope ref, not a $_ chain.
        SurfaceExpression::Field {
            expr: Some(inner), ..
        } => is_direct_underscore_surface(&inner.expr),
        SurfaceExpression::Field { expr: None, .. } => false,
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

    *node = Arc::new(SurfaceNode::new(
        SurfaceExpression::Fn {
            return_ann: None,
            params: vec![Spanned::new(
                SurfaceParam {
                    name: "_".to_string(),
                    annotation: None,
                    variadic: false,
                    resolved_annotation_type: crate::ast::TypeAnnotation::new(),
                },
                span.clone(),
            )],
            body,
            desugared: true,
        },
        span,
    ));
}

/// Recurse into all children of a SurfaceNode at the given depth.
///
/// For `Fn` nodes with `_` parameter, increment depth when recursing into the body
/// to suppress WRAP at depth > 0 (shadowing).
fn recurse_children_surface(node: &mut Arc<SurfaceNode>, depth: usize) {
    let node_mut = Arc::make_mut(node);

    match &mut node_mut.expr {
        // Literals and unannotated VarRef: no children to recurse into.
        SurfaceExpression::Int(_)
        | SurfaceExpression::U64(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::VarRef { annotation: None, .. }
        | SurfaceExpression::Placeholder(..)
        | SurfaceExpression::Decl(_) // type-level declaration, no evaluable children
        | SurfaceExpression::Error(_) => {}

        // StringLiteral: dispatch on prefix and delimiter for desugaring transformations.
        // - prefix == "i" AND delimiter.len() == 1: interpolated string → [tmpl "..." args...]
        // - prefix == "" AND delimiter.len() >= 3: triple-quoted → [unindent "..."]
        // - prefix == "i" AND delimiter.len() >= 3: both → [unindent [tmpl "..." args...]]
        // - otherwise: pass through to lowering unchanged
        //
        // Protocol note: "tmpl" and "unindent" are names that MUST be defined in any prelude
        // that supports interpolated and triple-quoted strings. They are protocol requirements
        // of the tinct Rust layer — the desugar pass unconditionally emits calls to these names.
        // D-3 tracks the formal decision on whether they should become Rust builtins or remain
        // prelude-defined, and how to handle user-provided preludes that omit them.
        SurfaceExpression::StringLiteral { prefix, delimiter, content } => {
            if prefix == "i" {
                // Interpolated string: build tmpl call
                let tmpl_node = build_interpolated_string_node(content, delimiter, &node_mut.span);
                if delimiter.len() >= 3 {
                    // Also triple-quoted: wrap in unindent
                    let unindent_wrapped = wrap_in_unindent_node(tmpl_node, &node_mut.span);
                    node_mut.expr = unindent_wrapped.expr.clone();
                } else {
                    node_mut.expr = tmpl_node.expr.clone();
                }
            } else if delimiter.len() >= 3 && prefix.is_empty() {
                // Triple-quoted (non-interpolated): wrap in unindent, keeping the delimiter
                // triple-quoted so lowering knows not to process escape sequences
                let inner = Arc::new(SurfaceNode::new(
                    SurfaceExpression::StringLiteral {
                        prefix: String::new(),
                        delimiter: delimiter.clone(),
                        content: content.clone(),
                    },
                    node_mut.span.clone(),
                ));
                let unindent_wrapped = wrap_in_unindent_node(inner, &node_mut.span);
                node_mut.expr = unindent_wrapped.expr.clone();
            }
            // else: plain single-quoted or unknown prefix — pass through to lowering
        }

        // Access expressions: recurse into target.
        // Leading-dot (expr: None) has no child to recurse into.
        SurfaceExpression::Field { expr: Some(target), .. } => {
            desugar_surface(target, depth);
        }
        SurfaceExpression::Field { expr: None, .. } => {}

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
            ..
        } => {
            desugar_surface_annotation(&mut annotation.node, depth);
            desugar_surface(inner, depth);
        }

        // Annotated VarRef: recurse into annotation (annotation is now on VarRef directly).
        SurfaceExpression::VarRef { annotation: Some(annotation), .. } => {
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
                for body_expr in &mut arm.body {
                    desugar_surface(body_expr, depth);
                }
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
        SurfaceExpression::VarRef { name, escaped, .. } => {
            // Bare name (e.g., `a | f`): call it with lhs as the sole argument.
            SurfaceExpression::Call {
                func: Arc::new(SurfaceNode::new(
                    SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped: *escaped,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                    },
                    rhs_stage.span.clone(),
                )),
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

    Arc::new(SurfaceNode::new(new_expr, span))
}

/// Desugar an optional annotation.
fn desugar_surface_annotation_option(ann: &mut Option<Spanned<Annotation>>, depth: usize) {
    if let Some(ann_spanned) = ann {
        desugar_surface_annotation(&mut ann_spanned.node, depth);
    }
}

/// Build a `[tmpl "template" ...]` call node from an interpolated string.
///
/// Processes the raw content character by character:
/// - `$$` → literal `$` in template (kept as `$$` for tmpl to interpret)
/// - `$` followed by identifier chars → variable reference: keep `$name` in template (tmpl macro
///   resolves it at compile time)
/// - `$` followed by nothing or non-identifier → pass through literally (keep `$` in template)
/// - Everything else → literal text in template
///
/// The `${expr}` form is not supported — there is no expression interpolation in tinct string
/// literals. Only `$ident` variable references are valid. See doc/quickstart.md §Strings.
///
/// The `delimiter` parameter must be the original string delimiter (e.g. `"` for single-quoted,
/// `"""` for triple-quoted). It is propagated to the inner StringLiteral so that lower.rs applies
/// escape processing only for single-quoted strings (`delimiter.len() == 1`) and passes content
/// raw for triple-quoted strings (`delimiter.len() >= 3`). Without this, an `i"""..."""` string
/// would have its backslashes escape-processed (wrong), while `"""..."""` would not (correct).
fn build_interpolated_string_node(
    content: &str,
    delimiter: &str,
    span: &crate::ast::Span,
) -> Arc<SurfaceNode> {
    let mut template = String::new();
    let mut chars = content.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            match chars.peek() {
                Some(&'$') => {
                    // $$ → literal $ in template (keep as $$ for tmpl to interpret)
                    template.push_str("$$");
                    chars.next(); // consume second $
                }
                Some(&c) if c.is_alphabetic() || c == '_' => {
                    // $identifier: keep as $name in template
                    template.push('$');
                    template.push(c);
                    chars.next(); // consume first char of identifier
                    while let Some(&c) = chars.peek() {
                        if c.is_alphanumeric() || c == '_' || c == '-' {
                            template.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                _ => {
                    // $ followed by nothing or non-identifier: pass through literally
                    template.push('$');
                }
            }
        } else {
            template.push(ch);
        }
    }

    // Build [tmpl "template"] — use the original delimiter so lower.rs applies escape processing
    // only for single-quoted i-strings and passes content raw for triple-quoted i-strings.
    let args = vec![Arc::new(SurfaceNode::new(
        SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: delimiter.to_string(),
            content: template,
        },
        span.clone(),
    ))];

    Arc::new(SurfaceNode::new(
        SurfaceExpression::Call {
            func: Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: "tmpl".to_string(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
                span.clone(),
            )),
            args,
            named_args: vec![],
            implied: true,
        },
        span.clone(),
    ))
}

/// Wrap a node in `[unindent node]`.
fn wrap_in_unindent_node(inner: Arc<SurfaceNode>, span: &crate::ast::Span) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(
        SurfaceExpression::Call {
            func: Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: "unindent".to_string(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
                span.clone(),
            )),
            args: vec![inner],
            named_args: vec![],
            implied: true,
        },
        span.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // B-409: desugar_instance_decls_expr must NOT transform multi-arm instances.
    //
    // Before this fix, the function used only arms[0] regardless of how many arms the instance
    // had, silently discarding all subsequent arms. Now it only transforms single-arm instances;
    // multi-arm instances are left as SurfaceExpression::Decl(InstanceDecl) so lower.rs can
    // emit all arms correctly via instance_binding_name.
    //
    // This test parses a dict containing two named instances — one single-arm and one
    // multi-arm — then applies desugar_instance_decls_surface_program, and verifies:
    //   1. The single-arm instance IS transformed to SurfaceExpression::Dict.
    //   2. The multi-arm instance is NOT transformed (still SurfaceExpression::Decl(InstanceDecl)).

    fn test_file(src: &str) -> Arc<crate::ast::SourceFile> {
        Arc::new(crate::ast::SourceFile {
            path: Arc::from(file!()),
            content: Arc::from(src),
        })
    }

    fn parse_program(src: &str) -> SurfaceProgram {
        crate::parser::parse(src, test_file(src))
            .unwrap_or_else(|e| panic!("parse failed: {e:?}"))
            .program
    }

    #[test]
    fn test_single_arm_instance_is_transformed() {
        // A single-arm named instance in dict-entry position should be transformed to a plain Dict.
        let mut program =
            parse_program("[MonadResult: [instance Monad [let m@Result]: [bind: [fn [let x] x]]]]");
        desugar_instance_decls_surface_program(&mut program);

        // Extract the first (and only) document's first expression item.
        let doc = &program.documents[0].node;
        let node = match doc.items.first().expect("expected one item") {
            SurfaceItem::Expr(n) => n,
            other => panic!("expected SurfaceItem::Expr, got {other:?}"),
        };

        // The outer dict should have one entry: MonadResult.
        match &node.expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected one entry");
                // The value should now be a plain Dict (not a Decl).
                match &entries[0].node.value.expr {
                    SurfaceExpression::Dict(_) => {} // correct: single-arm was transformed
                    SurfaceExpression::Decl(d) => {
                        panic!("single-arm instance was NOT transformed — still Decl: {d:?}")
                    }
                    other => panic!("expected Dict or Decl, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_multi_arm_instance_is_not_transformed() {
        // A multi-arm named instance must NOT be transformed — all arms must be preserved for
        // lower.rs to emit all instance-binding-name-keyed dict entries (B-409).
        let mut program = parse_program(
            r#"[AddableInts: [instance Addable
                [let a@Integer b@Integer c]:   [+: [fn [let x y] x]]
                [let a@Integer b@Float c]: [+: [fn [let x y] x]]]]"#,
        );
        desugar_instance_decls_surface_program(&mut program);

        let doc = &program.documents[0].node;
        let node = match doc.items.first().expect("expected one item") {
            SurfaceItem::Expr(n) => n,
            other => panic!("expected SurfaceItem::Expr, got {other:?}"),
        };

        match &node.expr {
            SurfaceExpression::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected one entry");
                // The value must still be a Decl(InstanceDecl) with both arms intact.
                match &entries[0].node.value.expr {
                    SurfaceExpression::Decl(decl) => {
                        if let SurfaceDeclaration::InstanceDecl { arms, .. } = decl.as_ref() {
                            assert_eq!(
                                arms.len(),
                                2,
                                "B-409: both arms must be preserved, got {} arms",
                                arms.len()
                            );
                        } else {
                            panic!("expected InstanceDecl, got {decl:?}");
                        }
                    }
                    SurfaceExpression::Dict(_) => {
                        panic!("B-409: multi-arm instance was incorrectly transformed to a plain Dict — all arms after arms[0] were lost")
                    }
                    other => panic!("expected Decl or Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }
}
