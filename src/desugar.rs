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
//!    asserts `Expr::Pipe` is unreachable (`typecheck.rs:2235`); the evaluator
//!    eliminates them via `expr_to_core_expr` in `ast_convert.rs` (which converts
//!    Pipe to Call silently). This module is the single lowering site for pipe expressions.
//!
//! **Desugar nesting depth invariant:** Desugar only transforms `$_` into fn wrappers
//! (one level per `$_` occurrence). Nesting depth is bounded by the parser's
//! MAX_PARSE_DEPTH, which is equal to MAX_EVAL_DEPTH (256). Therefore, desugaring
//! cannot produce ASTs deeper than the evaluation depth limit.
//!
//! See doc/04-functions.md §`$_` Desugaring for the complete formal specification.

use crate::ast::{
    Annotation, Spanned, SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceItem,
    SurfaceNode, SurfaceParam, SurfaceProgram,
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
    let span = node.span;
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
                span,
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
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Error(_) => {}

        // Access expressions: recurse into target
        SurfaceExpression::DotAccess { expr: target, .. } => {
            desugar_surface(target, depth);
        }

        // Pipe: recurse into both sides at the CURRENT depth, then rewrite the pipe itself.
        SurfaceExpression::Pipe { lhs, rhs } => {
            desugar_surface(lhs, depth);
            desugar_surface(rhs, depth);
            desugar_pipe_surface(node);
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
        SurfaceExpression::CaseArm { pattern, body } => {
            desugar_surface(pattern, depth);
            desugar_surface(body, depth);
        }
        SurfaceExpression::TypeApp { func, arg } => {
            desugar_surface(func, depth);
            desugar_surface(arg, depth);
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

/// Desugar a Pipe SurfaceNode by transforming it into a Call.
///
/// Rules:
/// - `Pipe(lhs, Call(f, args))` → `Call(f, args ++ [lhs])`
/// - `Pipe(lhs, VarRef(n))` → `Call(VarRef(n), [lhs])`
/// - `Pipe(lhs, other)` → `Call(other, [lhs])`
fn desugar_pipe_surface(node: &mut Arc<SurfaceNode>) {
    let node_mut = Arc::make_mut(node);

    // Extract lhs and rhs from the Pipe node
    let (lhs, rhs) = match &mut node_mut.expr {
        SurfaceExpression::Pipe { lhs, rhs } => {
            // Clone the Arc pointers to preserve ownership
            (Arc::clone(lhs), Arc::clone(rhs))
        }
        _ => return, // Not a Pipe, nothing to do
    };

    // Transform based on RHS type
    let new_expr = match &rhs.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // Append lhs as final positional argument
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
            // Bare word: call it with lhs as the only argument
            SurfaceExpression::Call {
                func: Arc::new(SurfaceNode {
                    expr: SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped: *escaped,
                    },
                    span: rhs.span,
                }),
                args: vec![lhs],
                named_args: vec![],
                implied: true,
            }
        }
        _ => {
            // Any other expression: call it with lhs
            SurfaceExpression::Call {
                func: rhs,
                args: vec![lhs],
                named_args: vec![],
                implied: true,
            }
        }
    };

    node_mut.expr = new_expr;
}

/// Desugar an optional annotation.
fn desugar_surface_annotation_option(ann: &mut Option<Spanned<Annotation>>, depth: usize) {
    if let Some(ann_spanned) = ann {
        desugar_surface_annotation(&mut ann_spanned.node, depth);
    }
}
