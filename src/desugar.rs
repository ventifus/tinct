//! `$_` desugaring — pre-typecheck AST transformation.
//!
//! This module implements the source-to-source transformation that rewrites
//! expressions containing `$_` (underscore placeholder) into explicit lambda
//! expressions. The desugaring runs after parsing and before both type checking
//! and evaluation.
//!
//! **Desugar nesting depth invariant:** Desugar only transforms `$_` into fn wrappers
//! (one level per `$_` occurrence). Nesting depth is bounded by the parser's
//! MAX_PARSE_DEPTH, which is equal to MAX_EVAL_DEPTH (256). Therefore, desugaring
//! cannot produce ASTs deeper than the evaluation depth limit.
//!
//! See doc/04-functions.md §`$_` Desugaring for the complete formal specification.

use crate::ast::{Annotation, Document, Entry, Expr, File, Param, Spanned};
use std::rc::Rc;

/// Desugar a complete file (all documents).
///
/// Runs the `$_` desugaring transformation on every expression in every document.
/// Mutates the AST in place.
pub fn desugar_file(file: &mut File) {
    for doc_spanned in &mut file.documents {
        desugar_document(&mut doc_spanned.node);
    }
}

/// Desugar a single document (all expressions).
fn desugar_document(doc: &mut Document) {
    for expr_rc in &mut doc.expressions {
        let expr_spanned = Rc::make_mut(expr_rc);
        desugar_expr(expr_spanned, 0);
    }
}

/// Desugar a single expression at the given lexical depth.
///
/// `depth` tracks how many enclosing `Fn([_] ...)` lambdas we are inside:
/// - `depth = 0`: `$_` is unbound, WRAP rules apply
/// - `depth > 0`: `$_` is bound by an enclosing lambda, only recurse (no wrapping)
///
/// This is the main entry point for desugaring a standalone expression (e.g., REPL input).
pub fn desugar_expr(expr: &mut Spanned<Expr>, depth: usize) {
    desugar(expr, depth);
}

/// Core desugaring logic: top-down traversal with selective wrapping.
///
/// At depth=0, check WRAP conditions on raw children BEFORE recursing.
/// If any child is DIRECT, wrap the whole expression in `[fn [_] ...]`.
/// Then recurse into children at depth+1 (inside the lambda, `$_` is bound).
///
/// At depth>0, only recurse into children (no wrapping).
///
/// Precondition: AST depth must be bounded (currently by parser's MAX_PARSE_DEPTH=256).
/// Programmatic AST construction (macros, quasiquoting) must respect this bound.
fn desugar(expr: &mut Spanned<Expr>, depth: usize) {
    // At depth 0, try to wrap based on raw children
    if depth == 0 {
        if try_wrap(expr) {
            // After wrapping, the body is at depth+1 (inside the generated lambda)
            // We need to recurse into the wrapped body
            if let Expr::Fn { body, .. } = &mut expr.node {
                desugar(Rc::make_mut(body), 1);
            }
            return;
        }
    }

    // No wrapping occurred (or depth > 0): recurse into children
    recurse_children(expr, depth);
}

/// Check if this expression should be wrapped based on DIRECT children.
///
/// Returns `true` if wrapping occurred, `false` otherwise.
/// Mutates `expr` in place by replacing it with `[fn [_] original_expr]`.
fn try_wrap(expr: &mut Spanned<Expr>) -> bool {
    match &expr.node {
        // WRAP-CALL: any arg (not func position) is DIRECT
        Expr::Call {
            func: _,
            args,
            named_args,
            implied: _,
        } => {
            // Func position excluded from WRAP check
            let has_direct_arg = args.iter().any(|a| is_direct_underscore(&a.node))
                || named_args
                    .iter()
                    .any(|na| is_direct_underscore(&na.node.value.node));

            if has_direct_arg {
                wrap_expr_in_lambda(expr);
                return true;
            }
            false
        }

        // WRAP-DICT: any value (not key) is DIRECT
        Expr::Dict(entries) => {
            let has_direct_value = entries
                .iter()
                .any(|e| is_direct_underscore(&e.node.value.node));

            if has_direct_value {
                wrap_expr_in_lambda(expr);
                return true;
            }
            false
        }

        // WRAP-DOT: target is DIRECT (single $_ or access chain on $_)
        Expr::DotAccess { expr: target, .. } => {
            if is_direct_underscore(&target.node) {
                wrap_expr_in_lambda(expr);
                return true;
            }
            false
        }

        // WRAP-PIPE: LHS is DIRECT (e.g., `$_ | f` becomes `[fn [_] $_ | f]`)
        // The desugar_pipe step inside the lambda body will then transform
        // `$_ | f` to `Call(f, [VarRef("_")])` at depth > 0 where `_` is bound.
        Expr::Pipe { lhs, .. } => {
            if is_direct_underscore(&lhs.node) {
                wrap_expr_in_lambda(expr);
                return true;
            }
            false
        }

        // All other cases: no wrapping
        _ => false,
    }
}

/// DIRECT predicate: tests whether an expression is `$_` or an access chain rooted at `$_`.
///
/// Access chain keys and dict entry keys are excluded — only the
/// access *target* triggers desugaring.
fn is_direct_underscore(expr: &Expr) -> bool {
    match expr {
        Expr::VarRef { name, .. } => name == "_",
        // Access chains on $_ count as DIRECT (e.g., $_.name)
        Expr::DotAccess { expr: inner, .. } => is_direct_underscore(&inner.node),
        // Pipe chains: check LHS (e.g., $_ | f becomes [fn [_] $_ | f])
        Expr::Pipe { lhs, .. } => is_direct_underscore(&lhs.node),
        // All other expressions: not DIRECT
        _ => false,
    }
}

/// Wrap an expression in `[fn [_] original_expr]`, reusing the original span.
///
/// Mutates `expr` in place using `std::mem::replace` to move the original node
/// into the lambda body.
fn wrap_expr_in_lambda(expr: &mut Spanned<Expr>) {
    let span = expr.span;
    let original_node = std::mem::replace(
        &mut expr.node,
        Expr::Int(0), // Dummy value; immediately overwritten after original_node is captured.
    );

    expr.node = Expr::Fn {
        return_ann: None,
        params: vec![Spanned::new(
            Param {
                name: "_".to_string(),
                annotation: None,
                variadic: false,
            },
            span,
        )],
        body: Rc::new(Spanned::new(original_node, span)),
        desugared: true,
    };
}

/// Recurse into all children of an expression at the given depth.
///
/// For `Fn` nodes with `_` parameter, increment depth when recursing into the body
/// to suppress WRAP at depth > 0 (shadowing).
fn recurse_children(expr: &mut Spanned<Expr>, depth: usize) {
    match &mut expr.node {
        // Literals: no children
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Rest(_)
        | Expr::Error(_) => {}

        // Access expressions: recurse into target
        Expr::DotAccess { expr: target, .. } => {
            desugar(target, depth);
        }
        // Pipe: recurse into both sides at the CURRENT depth, then rewrite the pipe itself.
        //
        // Why here (recurse_children) rather than in try_wrap?
        //   try_wrap handles only the WRAP-PIPE case (`$_ | f` at depth 0, where the whole
        //   pipe is wrapped in `[fn [_] ...]`). After wrapping, the lambda body is recursed
        //   at depth+1 — catching any child $_ references inside the body.
        //   recurse_children handles the non-wrapping case (any other pipe, or depth>0):
        //   recurse both sides so that nested $_ in lhs/rhs are desugared, then rewrite the
        //   Pipe node itself into a Call via desugar_pipe. This ordering (recurse children,
        //   THEN rewrite) ensures that the children of the Pipe are fully desugared before
        //   the Pipe disappears from the AST.
        Expr::Pipe { lhs, rhs } => {
            desugar(lhs, depth);
            desugar(rhs, depth);
            desugar_pipe(expr);
        }

        // Sequential: recurse into all expressions
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                if let Some(seq_expr_mut) = Rc::get_mut(seq_expr) {
                    desugar(seq_expr_mut, depth);
                }
            }
        }

        // Dict: recurse into keys and values
        Expr::Dict(entries) => {
            for entry_spanned in entries {
                desugar_entry(&mut entry_spanned.node, depth);
            }
        }

        // Call: recurse into func, args, and named args
        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => {
            desugar(func, depth);
            for arg in args {
                // Skip shared Rcs to avoid deep clone stack overflow (see desugar_entry comment)
                if let Some(arg_mut) = Rc::get_mut(arg) {
                    desugar(arg_mut, depth);
                }
            }
            for named_arg_spanned in named_args {
                // Skip shared Rcs to avoid deep clone stack overflow (see desugar_entry comment)
                if let Some(value_mut) = Rc::get_mut(&mut named_arg_spanned.node.value) {
                    desugar(value_mut, depth);
                }
            }
        }

        // Fn: increment depth if `_` is a parameter, then recurse into body
        Expr::Fn {
            params,
            body,
            return_ann,
            desugared: _,
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
                desugar_param_annotation(&mut param_spanned.node.annotation, depth);
            }

            // Recurse into return annotation (if it contains expressions)
            desugar_annotation_option(return_ann, depth);

            // Recurse into body at new depth
            // Skip shared Rcs to avoid deep clone stack overflow (see desugar_entry comment)
            if let Some(body_mut) = Rc::get_mut(body) {
                desugar(body_mut, new_depth);
            }
        }

        // TypeAlias: recurse into the aliased expression
        //
        // TypeAlias bodies are type expressions, not runtime expressions. $_ desugaring applies
        // here for consistency — `[type X $_.field]` desugars the implicit lambda — but this is
        // likely a user error since type expressions don't evaluate.
        Expr::TypeAlias { body, .. } => {
            desugar(body, depth);
        }

        // TypeAssert: recurse into annotation and expression
        Expr::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            desugar_annotation(&mut annotation.node, depth);
            desugar(inner, depth);
        }

        // Annotated: recurse into annotation
        Expr::Annotated { annotation, .. } => {
            desugar_annotation(&mut annotation.node, depth);
        }

        // Quote: DO NOT recurse into the quoted expression.
        // $_ inside a quote should remain as-is (AST frozen).
        Expr::Quote(_) => {}

        // Unquote and UnquoteSplice: DO recurse into the unquoted expression.
        // The expression inside [unquote ...] is evaluated in the current environment,
        // so $_ desugaring should apply.
        Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            desugar(&mut **inner, depth);
        }

        // Match: recurse into scrutinee and arm bodies (but not patterns).
        // Patterns don't contain runtime expressions, so they don't need desugaring.
        Expr::Match { scrutinee, arms } => {
            desugar(&mut **scrutinee, depth);
            for arm in arms {
                desugar(&mut *arm.body, depth);
            }
        }

        // DefMacro: desugar the transformer expression.
        Expr::DefMacro { transformer, .. } => {
            desugar(&mut **transformer, depth);
        }
    }
}

/// Desugar a dict entry (key and value).
fn desugar_entry(entry: &mut Entry, depth: usize) {
    if let Some(key_spanned) = &mut entry.key {
        desugar(key_spanned, depth);
    }
    // Try to get mutable access without cloning. Rc::get_mut returns Some only if
    // there's exactly one strong reference (no sharing). For shared Rcs (only created
    // by error recovery in parser.rs:925, 1483), skip desugaring to avoid deep clone
    // stack overflow. Error-recovery ASTs are already semantically broken, so skipping
    // $_ desugaring on them is acceptable.
    if let Some(value_mut) = Rc::get_mut(&mut entry.value) {
        desugar(value_mut, depth);
    }
}

/// Desugar an annotation (if it's a PropertyDict with expression values).
fn desugar_annotation(ann: &mut Annotation, depth: usize) {
    match ann {
        Annotation::Simple(_) => {}
        Annotation::PropertyDict(entries) => {
            for entry_spanned in entries {
                desugar_entry(&mut entry_spanned.node, depth);
            }
        }
    }
}

/// Desugar a Pipe expression by transforming it into a Call.
///
/// Rules:
/// - `Pipe(lhs, Call(f, args))` → `Call(f, args ++ [lhs])`
/// - `Pipe(lhs, VarRef(n))` → `Call(VarRef(n), [lhs])`
/// - `Pipe(lhs, other)` → `Call(other, [lhs])`
///
/// This transformation happens after `$_` desugaring and recursion into children.
fn desugar_pipe(expr: &mut Spanned<Expr>) {
    let span = expr.span;

    // Extract lhs and rhs from the Pipe node
    let (lhs, rhs) = match &mut expr.node {
        Expr::Pipe { lhs, rhs } => {
            // Take ownership by replacing with dummy values
            let lhs_box = std::mem::replace(lhs, Box::new(Spanned::new(Expr::Int(0), span)));
            let rhs_box = std::mem::replace(rhs, Box::new(Spanned::new(Expr::Int(0), span)));
            (*lhs_box, *rhs_box)
        }
        _ => return, // Not a Pipe, nothing to do
    };

    // Transform based on RHS type
    let new_node = match rhs.node {
        Expr::Call {
            func,
            mut args,
            named_args,
            implied,
        } => {
            // Append lhs as final positional argument
            args.push(Rc::new(lhs));
            Expr::Call {
                func,
                args,
                named_args,
                implied,
            }
        }
        Expr::VarRef { name, resolved } => {
            // Bare word: call it with lhs as the only argument
            Expr::Call {
                func: Box::new(Spanned::new(Expr::VarRef { name, resolved }, rhs.span)),
                args: vec![Rc::new(lhs)],
                named_args: vec![],
                implied: true,
            }
        }
        other_expr => {
            // Any other expression: call it with lhs
            Expr::Call {
                func: Box::new(Spanned::new(other_expr, rhs.span)),
                args: vec![Rc::new(lhs)],
                named_args: vec![],
                implied: true,
            }
        }
    };

    expr.node = new_node;
}

/// Desugar an optional annotation.
fn desugar_annotation_option(ann: &mut Option<Spanned<Annotation>>, depth: usize) {
    if let Some(ann_spanned) = ann {
        desugar_annotation(&mut ann_spanned.node, depth);
    }
}

/// Desugar a parameter's annotation (if present).
fn desugar_param_annotation(ann: &mut Option<Spanned<Annotation>>, depth: usize) {
    if let Some(ann_spanned) = ann {
        desugar_annotation(&mut ann_spanned.node, depth);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, NamedArg, Param};
    use crate::test_util::sp;

    /// Test the DIRECT predicate for VarRef("_")
    #[test]
    fn test_direct_underscore_var_ref() {
        let expr = Expr::var_ref("_".into());
        assert!(is_direct_underscore(&expr));
    }

    /// Test DIRECT predicate for non-underscore VarRef
    #[test]
    fn test_direct_underscore_var_ref_other() {
        let expr = Expr::var_ref("x".into());
        assert!(!is_direct_underscore(&expr));
    }

    /// Test DIRECT predicate for DotAccess chain on $_
    #[test]
    fn test_direct_underscore_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: crate::ast::DotKey::Ident("age".into()),
        };
        assert!(is_direct_underscore(&expr));
    }

    /// Test DIRECT predicate for nested DotAccess chain $_.user.name
    #[test]
    fn test_direct_underscore_nested_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("_".into()))),
                field: crate::ast::DotKey::Ident("user".into()),
            })),
            field: crate::ast::DotKey::Ident("name".into()),
        };
        assert!(is_direct_underscore(&expr));
    }

    /// Test DIRECT predicate for non-underscore access chain
    #[test]
    fn test_direct_underscore_dot_access_non_underscore() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("data".into()))),
            field: crate::ast::DotKey::Ident("age".into()),
        };
        assert!(!is_direct_underscore(&expr));
    }

    /// Test basic wrapping: `[call $f $_]` → `[fn [_] [call $f $_]]`
    #[test]
    fn test_wrap_call_with_direct_arg() {
        let mut expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
            named_args: vec![],
            implied: false,
        });

        desugar_expr(&mut expr, 0);

        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                match &body.node {
                    Expr::Call { func, args, .. } => {
                        assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "f"));
                        assert_eq!(args.len(), 1);
                        assert!(matches!(&args[0].node, Expr::VarRef { name, .. } if name == "_"));
                    }
                    _ => panic!("Expected Call in lambda body, got {:?}", body.node),
                }
            }
            _ => panic!("Expected Fn wrapper, got {:?}", expr.node),
        }
    }

    /// Test exclusion: func position NOT triggering wrap
    #[test]
    fn test_no_wrap_call_func_position() {
        let mut expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("_".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });

        desugar_expr(&mut expr, 0);

        // Should remain a Call (no wrapping)
        match &expr.node {
            Expr::Call { func, args, .. } => {
                assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "_"));
                assert_eq!(args.len(), 1);
            }
            _ => panic!("Expected Call, got {:?}", expr.node),
        }
    }

    /// Test shadowing: `[fn [_] [call $f $_]]` does NOT double-wrap
    #[test]
    fn test_no_double_wrap_shadowing() {
        let mut expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "_".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
                named_args: vec![],
                implied: false,
            })),
            desugared: false,
        });

        desugar_expr(&mut expr, 0);

        // Should remain a Fn with a Call body (no double wrapping)
        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                // Body should still be a Call (not wrapped in another Fn)
                match &body.node {
                    Expr::Call { func, args, .. } => {
                        assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "f"));
                        assert_eq!(args.len(), 1);
                    }
                    _ => panic!("Expected Call in body, got {:?}", body.node),
                }
            }
            _ => panic!("Expected Fn, got {:?}", expr.node),
        }
    }

    /// Test dict value wrapping: `[a: $_]` wraps
    #[test]
    fn test_wrap_dict_value() {
        let mut expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: Rc::new(sp(Expr::var_ref("_".into()))),
        })]));

        desugar_expr(&mut expr, 0);

        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                match &body.node {
                    Expr::Dict(entries) => {
                        assert_eq!(entries.len(), 1);
                        match &entries[0].node.value.node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "_"),
                            _ => panic!("Expected VarRef in dict value"),
                        }
                    }
                    _ => panic!("Expected Dict in lambda body"),
                }
            }
            _ => panic!("Expected Fn wrapper, got {:?}", expr.node),
        }
    }

    /// Test dict key exclusion: computed key `$_` does NOT wrap the dict
    #[test]
    fn test_no_wrap_dict_key() {
        let mut expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::var_ref("_".into()))),
            value: Rc::new(sp(Expr::Int(42))),
        })]));

        desugar_expr(&mut expr, 0);

        // Should remain a Dict (no wrapping, because key is not checked by WRAP-DICT)
        // BUT: the key itself should be recursed into, and since it's just `$_` at depth 0,
        // it stays as-is (VarRef doesn't wrap itself, only access chains and calls do)
        match &expr.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(
                    &entries[0].node.key.as_ref().unwrap().node,
                    Expr::VarRef { name, .. } if name == "_"
                ));
            }
            _ => panic!("Expected Dict, got {:?}", expr.node),
        }
    }

    /// Test WRAP-DOT: standalone access chain $_.field wraps
    #[test]
    fn test_wrap_dot_access() {
        let mut expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: crate::ast::DotKey::Ident("age".into()),
        });

        desugar_expr(&mut expr, 0);

        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                match &body.node {
                    Expr::DotAccess {
                        expr: target,
                        field,
                    } => {
                        assert!(matches!(&target.node, Expr::VarRef { name, .. } if name == "_"));
                        assert_eq!(*field, crate::ast::DotKey::Ident("age".into()));
                    }
                    _ => panic!("Expected DotAccess in lambda body"),
                }
            }
            _ => panic!("Expected Fn wrapper, got {:?}", expr.node),
        }
    }

    /// Test named args: `[call $f x: $_]` wraps
    #[test]
    fn test_wrap_call_named_arg() {
        let mut expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "x".into(),
                value: Rc::new(sp(Expr::var_ref("_".into()))),
            })],
            implied: false,
        });

        desugar_expr(&mut expr, 0);

        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                match &body.node {
                    Expr::Call {
                        func,
                        args,
                        named_args,
                        ..
                    } => {
                        assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "f"));
                        assert_eq!(args.len(), 0);
                        assert_eq!(named_args.len(), 1);
                        assert_eq!(named_args[0].node.name, "x");
                        assert!(matches!(
                            &named_args[0].node.value.node,
                            Expr::VarRef { name, .. } if name == "_"
                        ));
                    }
                    _ => panic!("Expected Call in lambda body"),
                }
            }
            _ => panic!("Expected Fn wrapper, got {:?}", expr.node),
        }
    }

    /// Test complex nesting: `[call $filter [call $> $_.age 30] $users]`
    /// The inner call should wrap, outer call should not
    #[test]
    fn test_nested_call_wrapping() {
        let mut expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("filter".into()))),
            args: vec![
                Rc::new(sp(Expr::Call {
                    func: Box::new(sp(Expr::var_ref(">".into()))),
                    args: vec![
                        Rc::new(sp(Expr::DotAccess {
                            expr: Box::new(sp(Expr::var_ref("_".into()))),
                            field: crate::ast::DotKey::Ident("age".into()),
                        })),
                        Rc::new(sp(Expr::Int(30))),
                    ],
                    named_args: vec![],
                    implied: false,
                })),
                Rc::new(sp(Expr::var_ref("users".into()))),
            ],
            named_args: vec![],
            implied: false,
        });

        desugar_expr(&mut expr, 0);

        // Outer call should remain a Call
        match &expr.node {
            Expr::Call { func, args, .. } => {
                assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "filter"));
                assert_eq!(args.len(), 2);

                // First arg should be a wrapped lambda
                match &args[0].node {
                    Expr::Fn { params, body, .. } => {
                        assert_eq!(params.len(), 1);
                        assert_eq!(params[0].node.name, "_");

                        // Body should be the inner call
                        match &body.node {
                            Expr::Call {
                                func: inner_func,
                                args: inner_args,
                                ..
                            } => {
                                assert!(matches!(
                                    &inner_func.node,
                                    Expr::VarRef { name, .. } if name == ">"
                                ));
                                assert_eq!(inner_args.len(), 2);
                                // First arg is $_.age
                                match &inner_args[0].node {
                                    Expr::DotAccess {
                                        expr: target,
                                        field,
                                    } => {
                                        assert!(matches!(
                                            &target.node,
                                            Expr::VarRef { name, .. } if name == "_"
                                        ));
                                        assert_eq!(*field, crate::ast::DotKey::Ident("age".into()));
                                    }
                                    _ => panic!("Expected DotAccess"),
                                }
                            }
                            _ => panic!("Expected Call in lambda body"),
                        }
                    }
                    _ => panic!("Expected Fn for first arg"),
                }

                // Second arg should remain VarRef("users")
                assert!(matches!(&args[1].node, Expr::VarRef { name, .. } if name == "users"));
            }
            _ => panic!("Expected Call, got {:?}", expr.node),
        }
    }

    /// Test file desugaring
    #[test]
    fn test_desugar_file() {
        let mut file = File {
            documents: vec![sp(Document {
                expressions: vec![
                    Rc::new(sp(Expr::Call {
                        func: Box::new(sp(Expr::var_ref("f".into()))),
                        args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
                        named_args: vec![],
                        implied: false,
                    })),
                    Rc::new(sp(Expr::Dict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("x".into()))),
                        value: Rc::new(sp(Expr::var_ref("_".into()))),
                    })]))),
                ],
                name: None,
                output_type: None,
                expects: None,
            })],
        };

        desugar_file(&mut file);

        // Both expressions should be wrapped
        let doc = &file.documents[0].node;
        assert_eq!(doc.expressions.len(), 2);

        // First expr: wrapped call
        match &doc.expressions[0].node {
            Expr::Fn { .. } => {}
            _ => panic!("Expected first expression to be wrapped"),
        }

        // Second expr: wrapped dict
        match &doc.expressions[1].node {
            Expr::Fn { .. } => {}
            _ => panic!("Expected second expression to be wrapped"),
        }
    }

    /// Test origin tagging: desugared lambdas get `desugared: true`,
    /// user-written lambdas keep `desugared: false`.
    /// Per Pombrio & Krishnamurthi (2014) Abstraction Property.
    #[test]
    fn test_desugared_origin_tag() {
        // Desugared lambda: [call $f $_] → [fn [_] [call $f $_]] with desugared=true
        let mut desugared_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
            named_args: vec![],
            implied: false,
        });
        desugar_expr(&mut desugared_expr, 0);
        match &desugared_expr.node {
            Expr::Fn { desugared, .. } => {
                assert!(
                    *desugared,
                    "synthetic lambda from $_ desugaring should have desugared=true"
                );
            }
            _ => panic!("Expected Fn wrapper"),
        }

        // User-written lambda: [fn [x] $x] keeps desugared=false
        let mut user_fn = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            desugared: false,
        });
        desugar_expr(&mut user_fn, 0);
        match &user_fn.node {
            Expr::Fn { desugared, .. } => {
                assert!(
                    !*desugared,
                    "user-written lambda should keep desugared=false"
                );
            }
            _ => panic!("Expected Fn to remain unchanged"),
        }
    }

    /// Test that nested desugaring: inner $_ wrapped with desugared=true,
    /// outer user Fn keeps desugared=false.
    #[test]
    fn test_desugared_nested_origin_tags() {
        // [fn [x] [call $f $_]] — outer is user-written, inner gets wrapped
        let mut expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
                named_args: vec![],
                implied: false,
            })),
            desugared: false,
        });

        desugar_expr(&mut expr, 0);

        // Outer Fn: user-written, desugared=false
        match &expr.node {
            Expr::Fn {
                desugared, body, ..
            } => {
                assert!(
                    !*desugared,
                    "outer user-written Fn should keep desugared=false"
                );

                // Inner: body should now be wrapped in a desugared Fn
                match &body.node {
                    Expr::Fn {
                        desugared: inner_desugared,
                        ..
                    } => {
                        assert!(
                            *inner_desugared,
                            "inner $_ wrapper should have desugared=true"
                        );
                    }
                    _ => panic!("Expected inner Fn wrapper from $_ desugaring"),
                }
            }
            _ => panic!("Expected outer Fn"),
        }
    }

    /// Test WRAP-CALL edge case: both func and arg are DIRECT
    ///
    /// `[call $_ $_]` wraps to `[fn [_] [call $_ $_]]` because WRAP-CALL
    /// only checks args (positional and named), not the func position.
    /// Both `$_` references then bind to the same generated parameter.
    #[test]
    fn test_wrap_call_both_direct() {
        let mut expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("_".into()))),
            args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
            named_args: vec![],
            implied: false,
        });

        desugar_expr(&mut expr, 0);

        // Should wrap because there IS a DIRECT arg (even though func is also DIRECT)
        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                // Body should be the original call with both positions referring to `$_`
                match &body.node {
                    Expr::Call { func, args, .. } => {
                        assert!(matches!(&func.node, Expr::VarRef { name, .. } if name == "_"));
                        assert_eq!(args.len(), 1);
                        assert!(matches!(&args[0].node, Expr::VarRef { name, .. } if name == "_"));
                    }
                    _ => panic!("Expected Call in lambda body, got {:?}", body.node),
                }
            }
            _ => panic!("Expected Fn wrapper, got {:?}", expr.node),
        }
    }

    /// Shadowing test: `_` is NOT bound by a param named `x` — `$_` in call arg wraps.
    ///
    /// `[fn [x] [call $f $_]]` — `x` is bound, but `_` is NOT a param.
    /// At depth 0 for the outer Fn body, `$_` in call arg is DIRECT → wraps inner call.
    /// Contrast with `test_no_double_wrap_shadowing` where `_` IS the param name.
    #[test]
    fn test_shadowing_only_applies_to_underscore_param() {
        let mut expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(), // NOT "_" — does not shadow `$_`
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![Rc::new(sp(Expr::var_ref("_".into())))], // $_ in arg position
                named_args: vec![],
                implied: false,
            })),
            desugared: false,
        });

        desugar_expr(&mut expr, 0);

        // Outer Fn: user-written, body should be a Fn wrapper for the inner $_ call
        match &expr.node {
            Expr::Fn {
                params,
                body,
                desugared,
                ..
            } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(!desugared, "outer fn should keep desugared=false");

                // Body should now be wrapped: [fn [_] [call $f $_]]
                match &body.node {
                    Expr::Fn {
                        params: inner_params,
                        body: inner_body,
                        desugared: inner_desugared,
                        ..
                    } => {
                        assert_eq!(inner_params.len(), 1);
                        assert_eq!(inner_params[0].node.name, "_");
                        assert!(inner_desugared, "inner lambda should be desugared=true");

                        match &inner_body.node {
                            Expr::Call { func, args, .. } => {
                                assert!(
                                    matches!(&func.node, Expr::VarRef { name, .. } if name == "f")
                                );
                                assert_eq!(args.len(), 1);
                                assert!(
                                    matches!(&args[0].node, Expr::VarRef { name, .. } if name == "_")
                                );
                            }
                            _ => panic!("Expected Call in inner lambda body"),
                        }
                    }
                    _ => panic!(
                        "Expected inner Fn wrapper from $_ desugaring, got {:?}",
                        body.node
                    ),
                }
            }
            _ => panic!("Expected outer Fn, got {:?}", expr.node),
        }
    }

    /// Dict entry value desugaring vs call arg desugaring: both wrap independently.
    ///
    /// `[a: $_ b: [call $f $_]]` at depth 0:
    /// - The dict has DIRECT value in entry `a:` → WRAP-DICT fires, wraps entire dict.
    /// - After wrapping, the body (at depth 1) recurses into the inner call.
    /// - The inner `[call $f $_]` at depth 1 has a DIRECT arg but depth > 0 → no further wrap.
    ///
    /// This tests the interaction between WRAP-DICT and nested call arg desugaring.
    #[test]
    fn test_dict_value_and_nested_call_arg_desugar() {
        let mut expr = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("a".into()))),
                value: Rc::new(sp(Expr::var_ref("_".into()))), // DIRECT value → WRAP-DICT fires
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("b".into()))),
                value: Rc::new(sp(Expr::Call {
                    func: Box::new(sp(Expr::var_ref("f".into()))),
                    args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
                    named_args: vec![],
                    implied: false,
                })),
            }),
        ]));

        desugar_expr(&mut expr, 0);

        // WRAP-DICT should fire: entire dict is wrapped in [fn [_] ...]
        match &expr.node {
            Expr::Fn {
                params,
                body,
                desugared,
                ..
            } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");
                assert!(desugared, "wrapping Fn should be desugared=true");

                match &body.node {
                    Expr::Dict(entries) => {
                        assert_eq!(entries.len(), 2);
                        // Entry a: $_  (still VarRef at depth 1, not wrapped further)
                        assert!(
                            matches!(&entries[0].node.value.node, Expr::VarRef { name, .. } if name == "_")
                        );
                        // Entry b: inner call at depth 1 — $_ is bound at depth 1,
                        // no further wrapping (WRAP-CALL only fires at depth 0)
                        match &entries[1].node.value.node {
                            Expr::Call { func, args, .. } => {
                                assert!(
                                    matches!(&func.node, Expr::VarRef { name, .. } if name == "f")
                                );
                                assert_eq!(args.len(), 1);
                                assert!(
                                    matches!(&args[0].node, Expr::VarRef { name, .. } if name == "_")
                                );
                            }
                            _ => panic!(
                                "Expected Call in dict entry b, got {:?}",
                                entries[1].node.value.node
                            ),
                        }
                    }
                    _ => panic!("Expected Dict in Fn body, got {:?}", body.node),
                }
            }
            _ => panic!("Expected Fn wrapper from WRAP-DICT, got {:?}", expr.node),
        }
    }

    /// Test $_ inside annotation within shadowing function
    ///
    /// `[fn [_] [fn [x: [@Number $_]] $x]]` — the outer `[fn [_] ...]` shadows `$_`,
    /// so the `$_` inside the annotation (at depth 1) should NOT trigger wrapping.
    /// This tests that annotations recurse at the current depth (not depth+1).
    #[test]
    fn test_underscore_in_annotation_inside_shadowing_fn() {
        let mut expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "_".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::Fn {
                return_ann: None,
                params: vec![sp(Param {
                    name: "x".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![
                        sp(Entry {
                            key: Some(sp(Expr::Str("type".into()))),
                            value: Rc::new(sp(Expr::var_ref("Number".into()))),
                        }),
                        sp(Entry {
                            key: None,
                            value: Rc::new(sp(Expr::var_ref("_".into()))),
                        }),
                    ]))),
                    variadic: false,
                })],
                body: Rc::new(sp(Expr::var_ref("x".into()))),
                desugared: false,
            })),
            desugared: false,
        });

        desugar_expr(&mut expr, 0);

        // Should remain unchanged — no wrapping because $_ is shadowed
        match &expr.node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");

                // Body should still be the inner Fn (not wrapped in another Fn)
                match &body.node {
                    Expr::Fn {
                        params: inner_params,
                        body: inner_body,
                        ..
                    } => {
                        assert_eq!(inner_params.len(), 1);
                        assert_eq!(inner_params[0].node.name, "x");

                        // Annotation should still have PropertyDict with VarRef("_")
                        match &inner_params[0].node.annotation {
                            Some(ann_spanned) => {
                                match &ann_spanned.node {
                                    Annotation::PropertyDict(entries) => {
                                        assert_eq!(entries.len(), 2);
                                        // Second entry should have VarRef("_") unchanged
                                        assert!(matches!(
                                            &entries[1].node.value.node,
                                            Expr::VarRef { name, .. } if name == "_"
                                        ));
                                    }
                                    _ => panic!("Expected PropertyDict annotation"),
                                }
                            }
                            None => panic!("Expected annotation on param x"),
                        }

                        // Body should be VarRef("x")
                        assert!(
                            matches!(&inner_body.node, Expr::VarRef { name, .. } if name == "x")
                        );
                    }
                    _ => panic!("Expected inner Fn in body, got {:?}", body.node),
                }
            }
            _ => panic!("Expected outer Fn, got {:?}", expr.node),
        }
    }

    // --- Pipe desugaring tests ---

    /// WRAP-PIPE: `$_ | f` wraps to `[fn [_] [call f _]]`.
    /// When the pipe LHS is DIRECT ($_ or access chain on $_), the whole pipe
    /// expression is wrapped in a lambda at depth 0.
    #[test]
    fn test_wrap_pipe_direct_lhs() {
        let mut expr = sp(Expr::Pipe {
            lhs: Box::new(sp(Expr::var_ref("_".into()))),
            rhs: Box::new(sp(Expr::var_ref("f".into()))),
        });

        desugar_expr(&mut expr, 0);

        // Should have wrapped in a lambda
        match &expr.node {
            Expr::Fn {
                params,
                body,
                desugared,
                ..
            } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "_");
                assert!(desugared, "WRAP-PIPE should produce desugared=true lambda");
                // Body: the pipe $_ | f desugared to Call(f, [$_])
                match &body.node {
                    Expr::Call { func, args, .. } => {
                        assert!(
                            matches!(&func.node, Expr::VarRef { name, .. } if name == "f"),
                            "expected func = VarRef(f), got {:?}",
                            func.node
                        );
                        assert_eq!(args.len(), 1);
                        assert!(
                            matches!(&args[0].node, Expr::VarRef { name, .. } if name == "_"),
                            "expected arg = VarRef(_), got {:?}",
                            args[0].node
                        );
                    }
                    _ => panic!(
                        "expected Call in WRAP-PIPE lambda body, got {:?}",
                        body.node
                    ),
                }
            }
            _ => panic!("expected Fn wrapper from WRAP-PIPE, got {:?}", expr.node),
        }
    }

    /// Non-WRAP-PIPE: `x | f` does NOT wrap — only DIRECT lhs triggers WRAP-PIPE.
    /// Instead it desugars directly to `Call(f, [x])`.
    #[test]
    fn test_no_wrap_pipe_non_direct_lhs() {
        let mut expr = sp(Expr::Pipe {
            lhs: Box::new(sp(Expr::var_ref("x".into()))),
            rhs: Box::new(sp(Expr::var_ref("f".into()))),
        });

        desugar_expr(&mut expr, 0);

        // Should NOT wrap — desugars directly to Call(f, [x])
        match &expr.node {
            Expr::Call { func, args, .. } => {
                assert!(
                    matches!(&func.node, Expr::VarRef { name, .. } if name == "f"),
                    "expected func = VarRef(f), got {:?}",
                    func.node
                );
                assert_eq!(args.len(), 1);
                assert!(
                    matches!(&args[0].node, Expr::VarRef { name, .. } if name == "x"),
                    "expected arg = VarRef(x), got {:?}",
                    args[0].node
                );
            }
            _ => panic!("expected Call from pipe desugar, got {:?}", expr.node),
        }
    }

    /// Pipe CALL-EXTEND: `x | [f a]` appends x as the last arg → `[f a x]`.
    #[test]
    fn test_pipe_call_extend() {
        let mut expr = sp(Expr::Pipe {
            lhs: Box::new(sp(Expr::var_ref("x".into()))),
            rhs: Box::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![Rc::new(sp(Expr::var_ref("a".into())))],
                named_args: vec![],
                implied: false,
            })),
        });

        desugar_expr(&mut expr, 0);

        // `x | [f a]` → `[f a x]` (lhs appended at end per rule)
        match &expr.node {
            Expr::Call { func, args, .. } => {
                assert!(
                    matches!(&func.node, Expr::VarRef { name, .. } if name == "f"),
                    "expected func = VarRef(f), got {:?}",
                    func.node
                );
                assert_eq!(args.len(), 2, "expected 2 args after pipe extend");
                // First arg is original: a
                assert!(
                    matches!(&args[0].node, Expr::VarRef { name, .. } if name == "a"),
                    "expected args[0] = VarRef(a), got {:?}",
                    args[0].node
                );
                // Second arg is the lhs appended: x
                assert!(
                    matches!(&args[1].node, Expr::VarRef { name, .. } if name == "x"),
                    "expected args[1] = VarRef(x), got {:?}",
                    args[1].node
                );
            }
            _ => panic!("expected Call from CALL-EXTEND pipe, got {:?}", expr.node),
        }
    }
}
