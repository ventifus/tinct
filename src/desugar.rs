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
            // Track already-injected constructor names to prevent duplicates when two
            // types in the same dict share a constructor name. Without deduplication,
            // the second injection would cause E030 "duplicate key" at runtime.
            let mut injected_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for se in entries {
                // Check if this entry's value is a TypeAlias Decl
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    if let SurfaceDeclaration::TypeAlias { body, .. } = decl.as_ref() {
                        // Extract type name from the dict entry key for tag qualification.
                        // Only Str and VarRef keys give us a stable type name.
                        // Computed keys are skipped (no injection for that entry).
                        // Absent keys (positional type declarations) use unqualified tags.
                        let type_name: Option<String> = match &se.node.key {
                            Some(key_node) => match &key_node.expr {
                                SurfaceExpression::Str(s) => Some(s.clone()),
                                SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
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
                                        // Already injected by a prior type in this dict — skip.
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

/// A constructor extracted from a TypeAlias body expression.
#[derive(Debug)]
struct AliasConstructor {
    /// Unqualified constructor name (e.g., `"Circle"`, `"Ok"`, `"None"`).
    name: String,
    /// Named field names, in order, for named-field constructors.
    /// Empty for unit constructors (bare VarRef, zero-arg Call, or positional-arg Call).
    fields: Vec<String>,
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
fn extract_surface_adt_ctors_from_expr(body: &SurfaceExpression) -> Vec<AliasConstructor> {
    let mut ctors = Vec::new();

    fn try_extract(expr: &SurfaceExpression, ctors: &mut Vec<AliasConstructor>) {
        match expr {
            // Bare uppercase VarRef → unit constructor
            SurfaceExpression::VarRef { name, .. } if crate::eval::is_constructor_name(name) => {
                ctors.push(AliasConstructor {
                    name: name.clone(),
                    fields: Vec::new(),
                });
            }
            // Call with uppercase func → unit or named-field constructor
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
                            });
                        } else {
                            // Named args → named-field constructor
                            let fields: Vec<String> =
                                named_args.iter().map(|na| na.node.name.clone()).collect();
                            ctors.push(AliasConstructor {
                                name: name.clone(),
                                fields,
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
            for entry in entries {
                if entry.node.key.is_none() {
                    // Positional entry in [type ...] body — may be a constructor
                    try_extract(&entry.node.value.expr, &mut ctors);
                }
                // Named-key entries are record-body type annotations, not constructors — skip.
            }
        }
        other => {
            // Single-entry type body (no dict wrapper)
            try_extract(other, &mut ctors);
        }
    }

    ctors
}

/// Build the SurfaceExpression value for an injected constructor entry.
///
/// - Unit constructor (`ctor.fields` is empty): `[variant "TypeName.CtorName"]`
/// - Named-field constructor (`ctor.fields` non-empty):
///   `[fn [...payload] [variant-payload "TypeName.CtorName" payload]]`
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
fn build_constructor_value(
    ctor: &AliasConstructor,
    qualified_tag: &str,
    syn_span: &Span,
) -> Arc<SurfaceNode> {
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
        Arc::new(SurfaceNode {
            expr: SurfaceExpression::Call {
                func: unit_variant_fn,
                args: vec![tag_arg],
                named_args: vec![],
                implied: false,
            },
            span: syn_span.clone(),
        })
    } else {
        // Named-field constructor: variadic fn that collects named args into a payload dict.
        //
        // Calling convention: the variadic `...payload` param collects all named args (e.g.,
        // `field1: val1 field2: val2`) into a single Dict at runtime, via the B-277 extension
        // to bind_args_thunks (C-NAMED-VALID amended: unmatched named args flow into variadic).
        //
        // Generates: [fn [...payload] [variant-payload "TypeName.CtorName" payload]]
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

        // Build [fn [...payload] [variant-payload "TypeName.CtorName" payload]]
        Arc::new(SurfaceNode {
            expr: SurfaceExpression::Fn {
                return_ann: None,
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
        SurfaceExpression::CaseArm { pattern, body } => {
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
