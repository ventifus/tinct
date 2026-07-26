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
//! MAX_PARSE_DEPTH (256), replaced by MAX_CONTINUATION_STACK (8192) in the CEK machine.
//! Therefore, desugaring cannot produce ASTs deeper than the parse depth limit.
//!
//! See doc/04-functions.md §`$_` Desugaring for the complete formal specification.

use crate::ast::{
    Annotation, Span, Spanned, SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceItem,
    SurfaceNode, SurfaceParam, SurfaceProgram,
};
use std::sync::Arc;

/// Desugar a complete SurfaceProgram (all documents).
///
/// Runs the `$_` desugaring transformation on every expression in every document.
/// Returns a new SurfaceProgram with desugared documents. Unchanged documents share
/// their Arc with the original (zero-copy for unmodified subtrees).
pub fn desugar_surface_program(program: &SurfaceProgram) -> SurfaceProgram {
    let documents = program
        .documents
        .iter()
        .map(|doc_spanned| {
            let new_doc = desugar_surface_document(&doc_spanned.node);
            Spanned::new(Arc::new(new_doc), doc_spanned.span.clone())
        })
        .collect();
    SurfaceProgram { documents }
}

/// Full desugar pipeline: `$_` lambda wrapping and Pipe → Call lowering.
pub fn desugar_program_full(program: &SurfaceProgram) -> SurfaceProgram {
    desugar_surface_program(program)
}

/// Desugar a single SurfaceDocument (all expression items).
/// Returns a new SurfaceDocument with desugared expression items.
fn desugar_surface_document(doc: &SurfaceDocument) -> SurfaceDocument {
    let items = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node_arc) => {
                let mut new_node = Arc::clone(node_arc);
                desugar_surface_node(&mut new_node, 0);
                SurfaceItem::Expr(new_node)
            }
            // Skip SurfaceItem::Decl — declarations are handled by the expander, not the evaluator
            other => other.clone(),
        })
        .collect();
    SurfaceDocument {
        header: doc.header.clone(),
        items,
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
            resolved_captures: crate::ast::CapturesCell::new(),
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
                desugar_surface_annotation_option(&mut param_spanned.node.annotation);
            }

            // Recurse into return annotation (if it contains expressions)
            desugar_surface_annotation_option(return_ann);

            // Recurse into body at new depth
            desugar_surface(body, new_depth);
        }

        // TypeAssert: recurse into annotation and expression
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            desugar_surface_annotation(&mut annotation.node);
            desugar_surface(inner, depth);
        }

        // Annotated VarRef: recurse into annotation (annotation is now on VarRef directly).
        SurfaceExpression::VarRef { annotation: Some(annotation), .. } => {
            desugar_surface_annotation(&mut annotation.node);
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
fn desugar_surface_annotation(ann: &mut Annotation) {
    match ann {
        Annotation::Simple(_) => {}
        Annotation::Quote => {}
        Annotation::PropertyDict(_entries) => {
            // PropertyDict entries use the old Entry/Expr AST types. $_ inside type
            // annotations is a user error (type expressions don't evaluate), so we
            // do not recurse here.
        }
        Annotation::Annotated(outer, inner) => {
            desugar_surface_annotation(outer);
            desugar_surface_annotation(inner);
        }
    }
}

/// Desugar a pipe chain starting at `node` (which must be a `Pipe` node).
///
/// Flattens the right-associative chain into stages, desugars each stage independently,
/// then left-folds them into nested `Call` nodes using `apply_pipe_step`.
///
/// For `a | b | c | d` (parsed as `Pipe(a, Pipe(b, Pipe(c, d)))`):
/// - Stages: `[(a, None), (b, Some(pipe1)), (c, Some(pipe2)), (d, Some(pipe3))]`
/// - After fold: `apply_pipe_step(apply_pipe_step(apply_pipe_step(a, b, pipe1), c, pipe2), d, pipe3)`
/// - Result: `[d [c [b a]]]` (correct left-associative nesting)
///
/// Each stage carries `Some(pipe_span)` where `pipe_span` is the span of the `|` token
/// that connects the preceding accumulator to this stage. The first stage carries `None`
/// because there is no preceding `|`.
fn desugar_pipe_chain(node: &mut Arc<SurfaceNode>, depth: usize) {
    // Collect all pipe stages with their preceding pipe-operator spans.
    let mut stages: Vec<(Arc<SurfaceNode>, Option<Span>)> = Vec::new();
    collect_pipe_stages(node, &mut stages);

    // Desugar each stage independently (not as part of a pipe chain).
    for (stage, _) in &mut stages {
        desugar_surface(stage, depth);
    }

    // Left-fold the stages: acc = stages[0], then for each subsequent stage,
    // acc = apply_pipe_step(acc, stage, pipe_span).
    debug_assert!(stages.len() >= 2, "Pipe node must have at least two stages");
    let mut stages_iter = stages.into_iter();
    let mut acc: Arc<SurfaceNode> = stages_iter.next().expect("at least one stage").0;
    for (step, pipe_span) in stages_iter {
        // pipe_span is always Some for stages after the first; fall back to the
        // step's own span only as a defensive measure (should never occur).
        let call_span = pipe_span.unwrap_or_else(|| step.span.clone());
        acc = apply_pipe_step(acc, step, call_span);
    }

    // Replace node's expression with the folded result.
    // The pipe_span on each generated Call carries the specific `|` operator's span;
    // lower_inner reads it to produce precise per-step error locations.
    Arc::make_mut(node).expr = acc.expr.clone();
}

/// Collect all stages of a right-associative pipe chain into a flat `Vec`.
///
/// Each entry is `(stage_node, preceding_pipe_span)`:
/// - First stage: `(node, None)` — no `|` precedes the initial value.
/// - Subsequent stages: `(node, Some(pipe_span))` where `pipe_span` is the span of the `|`
///   token in the `Pipe` node that connects the previous accumulator to this stage.
///
/// `Pipe(a, Pipe(b, Pipe(c, d)))` with pipe1/pipe2/pipe3 →
/// `[(a, None), (b, Some(pipe1)), (c, Some(pipe2)), (d, Some(pipe3))]`.
///
/// Only `Pipe` nodes are unwrapped; any non-`Pipe` node becomes a leaf stage.
fn collect_pipe_stages(
    node: &Arc<SurfaceNode>,
    stages: &mut Vec<(Arc<SurfaceNode>, Option<Span>)>,
) {
    if let SurfaceExpression::Pipe {
        lhs,
        rhs,
        pipe_span,
    } = &node.expr
    {
        // Recurse into lhs first (may itself be a Pipe chain).
        collect_pipe_stages(lhs, stages);
        // rhs is connected to the preceding accumulator by this pipe_span.
        // Tag the first entry rhs contributes with Some(pipe_span); if rhs is itself
        // a Pipe node, collect_pipe_stages will add lhs of that node next — we need
        // to attach pipe_span to the first stage that rhs contributes.
        let prev_len = stages.len();
        collect_pipe_stages(rhs, stages);
        // The first new entry from rhs gets tagged with this pipe_span.
        if stages.len() > prev_len {
            stages[prev_len].1 = pipe_span.clone();
        }
    } else {
        stages.push((Arc::clone(node), None));
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
            ..
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
                pipe_span: Some(span.clone()),
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
                        do_infer_placeholder: false,
                    },
                    rhs_stage.span.clone(),
                )),
                args: vec![lhs],
                named_args: vec![],
                implied: true,
                pipe_span: Some(span.clone()),
            }
        }
        _ => {
            // Any other desugared expression: call it with lhs.
            SurfaceExpression::Call {
                func: rhs_stage,
                args: vec![lhs],
                named_args: vec![],
                implied: true,
                pipe_span: Some(span.clone()),
            }
        }
    };

    Arc::new(SurfaceNode::new(new_expr, span))
}

/// Desugar an optional annotation.
fn desugar_surface_annotation_option(ann: &mut Option<Spanned<Annotation>>) {
    if let Some(ann_spanned) = ann {
        desugar_surface_annotation(&mut ann_spanned.node);
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
                    do_infer_placeholder: false,
                },
                span.clone(),
            )),
            args,
            named_args: vec![],
            implied: true,
            pipe_span: None,
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
                    do_infer_placeholder: false,
                },
                span.clone(),
            )),
            args: vec![inner],
            named_args: vec![],
            implied: true,
            pipe_span: None,
        },
        span.clone(),
    ))
}
