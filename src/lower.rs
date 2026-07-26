//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `SurfaceNode` — all cross-phase data lives inline on nodes.
//! De Bruijn coordinates are read from the inline `resolution` field on VarRef/Field nodes.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `Placeholder` (unresolvable — diagnostic emitted)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (with resolved_type from the inline TypeAnnotation field or Type::Unknown)
//! - `Field` → `Call(builtin-get, [Str/Int(key), target])` (unified key-based lookup)
//! - `SurfaceNode.type_guard` set → wraps the lowered CoreExpr in `CoreExpr::TypeAssert`
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::Arc;

use crate::ast::{
    class_decl_name, CoreEntry, CoreExpr, CoreMatchArm, CoreNamedArg, CoreParam, Span, Spanned,
    SurfaceEntry, SurfaceExpression, SurfaceNode, VarAddr,
};
use crate::rust_span;

/// Severity of a diagnostic emitted during lowering.
#[derive(Debug, Clone)]
pub enum LowerDiagnosticKind {
    Error,
}

/// A diagnostic produced during the lowering phase.
///
/// Lowering errors (unresolvable variables, parse errors) are reported via this diagnostic
/// and the corresponding expression is replaced with `CoreExpr::Placeholder`. Callers
/// that need eager error reporting (e.g., document loading) inspect the returned diagnostic
/// vec rather than waiting for the placeholder to be forced at runtime.
#[derive(Debug, Clone)]
#[must_use = "lower diagnostics must be checked; use lower_errors_to_eval_error to convert or explicitly drop with let _ = ..."]
pub struct LowerDiagnostic {
    pub kind: LowerDiagnosticKind,
    pub message: String,
    pub span: crate::ast::Span,
}

/// Resolve a mangled instance binding name to (level, slot) De Bruijn coordinates
/// by searching the accumulated resolver scope frames.
///
/// `frames[0]` is the outermost scope (root builtins); `frames[n-1]` is the innermost.
/// De Bruijn level 0 is the innermost scope (closest ancestor), so we search from the
/// innermost frame outward and return the offset as the level.
///
/// Returns `None` if the name is not found in any frame.
pub(crate) fn resolve_name_in_frames(
    frames: &[indexmap::IndexMap<String, u32>],
    name: &str,
) -> Option<(u32, u32)> {
    // frames[0] = outermost, frames[n-1] = innermost
    // level 0 = innermost → frames[n-1]; level k = frames[n-1-k]
    for (offset, frame) in frames.iter().rev().enumerate() {
        if let Some(&slot) = frame.get(name) {
            return Some((offset as u32, slot));
        }
    }
    None
}

/// Process escape sequences in a single-quoted string literal.
///
/// Recognized escapes:
/// - `\n` → newline
/// - `\t` → tab
/// - `\r` → carriage return
/// - `\\` → backslash
/// - `\<delimiter-char>` → that char (e.g., `\"` → `"` for delimiter `"`)
/// - `\` + anything else → pass through literally as backslash + that char
/// - trailing `\` → pass through as backslash
pub(crate) fn process_escapes(content: &str, delimiter: &str) -> String {
    let mut result = String::new();
    let mut chars = content.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&'n') => {
                    result.push('\n');
                    chars.next();
                }
                Some(&'t') => {
                    result.push('\t');
                    chars.next();
                }
                Some(&'r') => {
                    result.push('\r');
                    chars.next();
                }
                Some(&'\\') => {
                    result.push('\\');
                    chars.next();
                }
                Some(&c) if delimiter.starts_with(c) => {
                    // \<delimiter-char> → that char (e.g., \" → " for delimiter="\"")
                    result.push(c);
                    chars.next();
                }
                Some(&c) => {
                    // Unknown escape: pass through literally (backslash + char)
                    result.push('\\');
                    result.push(c);
                    chars.next();
                }
                None => {
                    // Trailing backslash: pass through
                    result.push('\\');
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Convert de Bruijn (level, slot) coordinates to a `VarAddr`.
///
/// Used for sources that still produce (level, slot) pairs rather than VarAddr directly:
/// - `resolve_name_in_frames` results (scope frame lookups for spread-dict `merge`)
/// - `check_constraints_on_var` in `type_unify.rs` (converts before writing to `CallDispatch`)
/// - Synthetic addresses for lowerer-generated nodes (constructor functions, builtin-make-annotated)
///
/// Resolver-produced `Resolution` cells now store `VarAddr` directly and do not use this function.
/// `CallDispatch` now stores `VarAddr` directly — the conversion happens in `type_unify.rs` at
/// `call_dispatch.set(debruijn_to_var_addr(level, slot))`, not in the lowerer.
///
/// Mapping:
/// - level=0 → `VarAddr::LetrecGroupMember { depth: 0, slot }` (current letrec group)
/// - level>0 → `VarAddr::ClosureCapture(slot)` (outer scope)
pub(crate) fn debruijn_to_var_addr(level: u32, slot: u32) -> VarAddr {
    if level == 0 {
        VarAddr::LetrecGroupMember { depth: 0, slot }
    } else {
        VarAddr::ClosureCapture(slot)
    }
}

/// Lower a single surface node to a CoreExpr, collecting diagnostics.
///
/// This is the entry point for per-thunk lowering. Called eagerly during `builtin-lower`
/// (the discrete lowering pipeline step) and from other callers that need a CoreExpr.
///
/// Lowering errors (unresolvable variables, malformed AST) are reported as `LowerDiagnostic`
/// entries in the returned vec and the corresponding expression is replaced with
/// `CoreExpr::Placeholder`. Callers that need eager error reporting (e.g., document loading)
/// inspect the diagnostic vec; callers that discard it accept that the placeholder will error
/// at runtime if forced.
///
/// All cross-phase data (type annotations, field slots, provenance) is read from inline
/// fields on the AST nodes — no external tables are consulted.
///
/// `scope_frames` — when `Some`, provides the accumulated resolver scope frames from the
/// init program's resolver run. Used to resolve `call_dispatch` mangled instance binding
/// names to correct De Bruijn coordinates. Pass `None` when the EvalContext was not
/// initialized via `with_scope_frames()` (test contexts, bootstrap paths).
pub fn lower(
    arc: &Arc<SurfaceNode>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> (Spanned<CoreExpr>, Vec<LowerDiagnostic>) {
    let mut diagnostics = Vec::new();
    let spanned = lower_inner(arc, &mut diagnostics, scope_frames);
    (spanned, diagnostics)
}

/// Internal lowering entry point that threads the diagnostics accumulator and scope frames.
///
/// Used by recursive calls within lower.rs and by eval machinery that does not need the
/// diagnostic Vec. When a VarRef or parse error is encountered, a `LowerDiagnostic` is
/// pushed and `CoreExpr::Placeholder` is emitted. Produces the same `Spanned<CoreExpr>`
/// as the public `lower()`.
///
/// `scope_frames` is threaded through all recursive calls so that `call_dispatch` rewrites
/// anywhere in the AST subtree can resolve the mangled instance binding name to correct
/// De Bruijn coordinates. Pass `None` when scope frames are not available.
pub(crate) fn lower_inner(
    arc: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<LowerDiagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> Spanned<CoreExpr> {
    // Use the pipe_span from a pipe-desugared Call if present — it points at the specific
    // `|` operator rather than the outer bracket. Falls back to the node's own span.
    let span = if let SurfaceExpression::Call {
        pipe_span: Some(ref ps),
        ..
    } = arc.expr
    {
        ps.clone()
    } else {
        arc.span.clone()
    };
    let core_expr = lower_expr(arc, &arc.expr, diagnostics, scope_frames);

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

/// Resolve an annotation name to a Type for TypeAssert pattern coverage.
///
/// Used by coverage.rs when the resolved_type OnceLock has no entry (--no-typecheck,
/// macros). Only Rust-level primitive types are recognized here. Prelude-defined types
/// (Boolean, Seq, etc.) are NOT hardcoded -- they go through the type checker's TyCon
/// registry when available, and fall through to Unknown otherwise.
/// Unknown is the accept-all fallback for unrecognized names.
pub(crate) fn annotation_name_to_type(name: &str) -> crate::type_def::Type {
    use crate::type_def::Type;
    match name {
        "Int" => Type::Int,
        "Float" => Type::Float,
        "String" | "Str" => Type::Str,
        "Bytes" => Type::Bytes,
        "Proxy" => Type::Proxy,
        // Variadic 0-required-param function = any callable (Function or Builtin).
        "Fn" | "Function" | "Builtin" => Type::Function {
            params: vec![],
            ret: Box::new(Type::Any),
            typed_variadics: vec![],
            rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
            required_count: 0,
        },
        // All other names (Boolean, Seq, user types, type variables, etc.)
        // fall through to Unknown — prelude-defined types are resolved by the
        // type checker, not hardcoded here.
        _ => Type::Unknown,
    }
}

// lower_pattern deleted (T-1750) — match arm patterns are now Arc<SurfaceNode>, passed through unchanged.

fn lower_expr(
    arc: &Arc<SurfaceNode>,
    expr: &SurfaceExpression,
    diagnostics: &mut Vec<LowerDiagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int(*n),
        SurfaceExpression::U64(n) => CoreExpr::U64(*n),
        SurfaceExpression::Float(n) => CoreExpr::Float(*n),
        SurfaceExpression::StringLiteral {
            prefix,
            delimiter,
            content,
        } => {
            debug_assert!(
                prefix.is_empty(),
                "i-strings (prefix='{}') must be desugared before lowering reaches StringLiteral",
                prefix
            );
            if delimiter.len() == 1 {
                // Single-quoted: process escape sequences
                CoreExpr::Str(process_escapes(content, delimiter))
            } else {
                // Triple-quoted (or longer): no escape processing
                // Content is already raw — pass through as-is
                CoreExpr::Str(content.clone())
            }
        }

        SurfaceExpression::VarRef {
            name,
            resolution,
            annotation,
            ..
        } => {
            match resolution.get() {
                Some(None) => {
                    // Name not in scope — resolver already emitted a diagnostic (or
                    // intentionally suppressed it in pattern position). Produce a
                    // Placeholder so downstream code sees a wildcard; the resolver
                    // owns any error reporting.
                    CoreExpr::Placeholder
                }
                Some(Some(addr)) => CoreExpr::Var {
                    name: name.clone(),
                    addr: addr.clone(),
                    annotation: annotation.clone(),
                },
                None => {
                    // Resolver did not run on this VarRef (OnceLock unset = internal error).
                    // This occurs if the resolver skipped this node entirely — e.g., inside a
                    // Quote body that wasn't suppressed, or a newly constructed node that bypassed
                    // the resolver pass. Emit a diagnostic and produce a Placeholder so the error
                    // surfaces at force time rather than silently producing wrong output.
                    let message = format!("undefined variable: {}", name);
                    diagnostics.push(LowerDiagnostic {
                        kind: LowerDiagnosticKind::Error,
                        message,
                        span: arc.span.clone(),
                    });
                    CoreExpr::Placeholder
                }
            }
        }

        SurfaceExpression::Field {
            expr: Some(inner),
            field,
            resolution,
            ..
        } => {
            // Build the getter function Var and the key argument.
            // The resolver writes a VarAddr for builtin-get into Field.resolution.
            // All dot-access desugars to [builtin-get key target] — one correct path.
            let getter_addr = match resolution.get() {
                Some(Some(addr)) => addr.clone(),
                state @ (Some(None) | None) => {
                    let why = if state.is_none() {
                        "resolver did not run on this node"
                    } else {
                        "builtin-get not found in any scope (resolver ran but returned None)"
                    };
                    diagnostics.push(LowerDiagnostic {
                        kind: LowerDiagnosticKind::Error,
                        message: format!(
                            "builtin-get: missing resolver coordinates for `.{}` — {}",
                            field, why
                        ),
                        span: arc.span.clone(),
                    });
                    return CoreExpr::Placeholder;
                }
            };

            let key_core = match field {
                crate::ast::DotKey::Int(n) => CoreExpr::Int(*n),
                crate::ast::DotKey::Ident(s) => CoreExpr::Str(s.clone()),
            };

            let getter_var = Arc::new(crate::ast::Spanned::new(
                CoreExpr::Var {
                    name: "builtin-get".to_string(),
                    addr: getter_addr,
                    annotation: None,
                },
                arc.span.clone(),
            ));
            let key_node = Arc::new(crate::ast::Spanned::new(key_core, arc.span.clone()));
            let target_node = Arc::new(lower_inner(inner, diagnostics, scope_frames));

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
            Some(Some(addr)) => CoreExpr::Var {
                name: name.clone(),
                addr: addr.clone(),
                annotation: None,
            },
            Some(None) => {
                diagnostics.push(LowerDiagnostic {
                    kind: LowerDiagnosticKind::Error,
                    message: format!("undefined variable: .{}", name),
                    span: arc.span.clone(),
                });
                CoreExpr::Placeholder
            }
            None => {
                // Resolver ran but did not set coordinates for this leading-dot reference.
                // This happens when the resolver skipped this node's enclosing scope
                // (e.g. inside a TypeAlias body). The name cannot be resolved — emit
                // a diagnostic rather than silently producing a MAX/MAX sentinel.
                diagnostics.push(LowerDiagnostic {
                    kind: LowerDiagnosticKind::Error,
                    message: format!("undefined variable: .{}", name),
                    span: arc.span.clone(),
                });
                CoreExpr::Placeholder
            }
        },

        // Leading-dot with integer key: `.0` — no parent-scope numeric lookup. The parser
        // rejects this at parse time, so this is a safety fallback only.
        SurfaceExpression::Field {
            expr: None,
            field: crate::ast::DotKey::Int(_),
            ..
        } => {
            diagnostics.push(LowerDiagnostic {
                kind: LowerDiagnosticKind::Error,
                message: "leading-dot integer access is not supported".to_string(),
                span: arc.span.clone(),
            });
            CoreExpr::Placeholder
        }

        // Pipe is syntactic sugar — rewrite to Call(rhs, [lhs]) so the evaluator
        // sees only Call nodes. Equivalent to: f |> g  ==  g(f).
        SurfaceExpression::Pipe { lhs, rhs, .. } => CoreExpr::Call {
            func: Arc::new(lower_inner(rhs, diagnostics, scope_frames)),
            args: vec![Arc::new(lower_inner(lhs, diagnostics, scope_frames))],
            named_args: vec![],
            implied: true,
        },

        SurfaceExpression::Sequential(exprs) => CoreExpr::Sequential(
            exprs
                .iter()
                .map(|e| Arc::new(lower_inner(e, diagnostics, scope_frames)))
                .collect(),
        ),

        SurfaceExpression::Dict(entries) => {
            // Check for spread entries (...expr) — desugar to merge calls.
            // [a: 1  b: 2  ...rest  c: 3] → merge(merge([a: 1  b: 2], rest), [c: 3])
            let has_rest = entries.iter().any(|e| {
                e.node.key.is_none()
                    && matches!(&e.node.value.expr, SurfaceExpression::Placeholder(..))
            });
            if has_rest {
                // Collect entry indices between rest markers.
                // segments[i] = indices of regular entries before rest_nodes[i].
                let mut segments: Vec<Vec<usize>> = vec![vec![]];
                let mut rest_indices: Vec<usize> = vec![];
                for (idx, se) in entries.iter().enumerate() {
                    if se.node.key.is_none() {
                        if let SurfaceExpression::Placeholder(..) = &se.node.value.expr {
                            rest_indices.push(idx);
                            segments.push(vec![]);
                            continue;
                        }
                    }
                    segments.last_mut().unwrap().push(idx);
                }

                // Lower a group of regular entry indices to CoreExpr::Dict.
                macro_rules! lower_seg {
                    ($idxs:expr) => {{
                        let mut ces: Vec<Spanned<CoreEntry>> = vec![];
                        for &i in $idxs.iter() {
                            let se = &entries[i];
                            let key = se.node.key.as_ref().map(|k| {
                                let lowered = match &k.expr {
                                    SurfaceExpression::VarRef {
                                        name,
                                        escaped: false,
                                        ..
                                    } => CoreExpr::Str(name.clone()),
                                    _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                                };
                                Arc::new(Spanned::new(lowered, k.span.clone()))
                            });
                            let value =
                                Arc::new(lower_inner(&se.node.value, diagnostics, scope_frames));
                            ces.push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                        }
                        CoreExpr::Dict(ces)
                    }};
                }

                // Build nested merge calls left-associatively.
                // acc starts as the first segment dict, then folds over (rest, next_seg) pairs.
                let span = arc.span.clone();

                // Resolve `merge` through scope_frames to get correct de Bruijn coordinates.
                let (merge_level, merge_slot) = match scope_frames
                    .and_then(|frames| resolve_name_in_frames(frames, "merge"))
                {
                    Some(coords) => coords,
                    None => {
                        diagnostics.push(LowerDiagnostic {
                            kind: LowerDiagnosticKind::Error,
                            message: "spread-dict desugaring: 'merge' not found in scope frames"
                                .to_string(),
                            span: span.clone(),
                        });
                        return CoreExpr::Placeholder;
                    }
                };

                let mut acc = lower_seg!(&segments[0]);
                for (i, &ri) in rest_indices.iter().enumerate() {
                    // lower_inner returns Spanned<CoreExpr> — wrap in Arc directly.
                    let rest_spanned =
                        lower_inner(&entries[ri].node.value, diagnostics, scope_frames);
                    // merge(acc, rest)
                    acc = CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: "merge".to_string(),
                                addr: debruijn_to_var_addr(merge_level, merge_slot),
                                annotation: None,
                            },
                            span.clone(),
                        )),
                        args: vec![
                            Arc::new(Spanned::new(acc, span.clone())),
                            Arc::new(rest_spanned),
                        ],
                        named_args: vec![],
                        implied: false,
                    };
                    // merge(acc, next_segment) if non-empty
                    if i + 1 < segments.len() && !segments[i + 1].is_empty() {
                        let seg = lower_seg!(&segments[i + 1]);
                        acc = CoreExpr::Call {
                            func: Arc::new(Spanned::new(
                                CoreExpr::Var {
                                    name: "merge".to_string(),
                                    addr: debruijn_to_var_addr(merge_level, merge_slot),
                                    annotation: None,
                                },
                                span.clone(),
                            )),
                            args: vec![
                                Arc::new(Spanned::new(acc, span.clone())),
                                Arc::new(Spanned::new(seg, span.clone())),
                            ],
                            named_args: vec![],
                            implied: false,
                        };
                    }
                }
                return acc;
            }

            // Collect explicit (non-ClassDecl) keyed names so that ClassDecl method
            // slot injection skips names that have explicit user definitions (e.g.
            // `=: [fn [let x y] Boolean.False]` should not be overwritten by a
            // ClassDecl method slot for `=`).
            let explicit_keys: std::collections::HashSet<String> = entries.iter().filter_map(|se| {
                let key_node = se.node.key.as_ref()?;
                let is_class_decl = matches!(
                    &se.node.value.expr,
                    SurfaceExpression::Decl(d) if matches!(d.as_ref(), crate::ast::SurfaceDeclaration::ClassDecl { .. })
                );
                if is_class_decl { return None; }
                match &key_node.expr {
                    SurfaceExpression::VarRef { name, escaped: false, .. } => Some(name.clone()),
                    SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
                    _ => None,
                }
            }).collect();

            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            // Shared across all InstanceDecl entries in this dict — prevents duplicate plain
            // method slots when multiple instances implement the same method (e.g. two
            // [instance File.Writable ...] entries both having `write:`).
            let mut emitted_instance_method_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for se in entries {
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                            {
                                // Both named and anonymous instances:
                                // 1. If named, emit the outer key binding first.
                                // 2. Emit plain method slots (resolver call-site resolution).
                                // 3. Emit mangled binding slots (dispatch).
                                // Order matches surface_dict_static_keys exactly.
                                if let Some(k) = &se.node.key {
                                    let lowered = lower_expr(
                                        &se.node.value,
                                        &se.node.value.expr,
                                        diagnostics,
                                        scope_frames,
                                    );
                                    let key_expr = match &k.expr {
                                        SurfaceExpression::VarRef {
                                            name,
                                            escaped: false,
                                            ..
                                        } => CoreExpr::Str(name.clone()),
                                        _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                                    };
                                    let key =
                                        Some(Arc::new(Spanned::new(key_expr, k.span.clone())));
                                    let value = Arc::new(Spanned::new(lowered, se.span.clone()));
                                    core_entries.push(Spanned::new(
                                        CoreEntry { key, value },
                                        se.span.clone(),
                                    ));
                                }
                                for (pattern, method_entries) in arms {
                                    let dispatch_tags = extract_dispatch_tags(&pattern.expr);
                                    let type_args: Vec<&str> =
                                        dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();
                                    for me in method_entries {
                                        let method_name = match me.node.key.as_ref() {
                                            Some(key_node) => match &key_node.expr {
                                                SurfaceExpression::StringLiteral {
                                                    content,
                                                    ..
                                                } => content.clone(),
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    name.clone()
                                                }
                                                _ => continue,
                                            },
                                            None => continue,
                                        };
                                        // Plain method slot — de-duplicated across ALL instances
                                        // in this dict (shared set prevents duplicate keys when
                                        // multiple instances implement the same method).
                                        if !explicit_keys.contains(&method_name)
                                            && emitted_instance_method_names
                                                .insert(method_name.clone())
                                        {
                                            let key = Some(Arc::new(Spanned::new(
                                                CoreExpr::Str(method_name.clone()),
                                                se.span.clone(),
                                            )));
                                            let value = Arc::new(Spanned::new(
                                                CoreExpr::Dict(vec![]),
                                                se.span.clone(),
                                            ));
                                            core_entries.push(Spanned::new(
                                                CoreEntry { key, value },
                                                se.span.clone(),
                                            ));
                                        }
                                        // Mangled binding slot for dispatch.
                                        let binding_name = crate::type_def::instance_binding_name(
                                            &class_decl_name(class_name),
                                            &method_name,
                                            &type_args,
                                        );
                                        let key = Some(Arc::new(Spanned::new(
                                            CoreExpr::Str(binding_name),
                                            se.span.clone(),
                                        )));
                                        let value = Arc::new(lower_inner(
                                            &me.node.value,
                                            diagnostics,
                                            scope_frames,
                                        ));
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
                            let ctor_dict = lower_type_alias_to_constructor_dict(
                                type_name_opt,
                                body,
                                diagnostics,
                                scope_frames,
                            );
                            let key = se.node.key.as_ref().map(|k| {
                                let lowered = match &k.expr {
                                    // Both plain and annotated VarRef use the name field.
                                    SurfaceExpression::VarRef { name, .. } => {
                                        CoreExpr::Str(name.clone())
                                    }
                                    _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                                };
                                Arc::new(Spanned::new(lowered, k.span.clone()))
                            });
                            let value = Arc::new(Spanned::new(ctor_dict, se.span.clone()));
                            core_entries
                                .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                        }
                        crate::ast::SurfaceDeclaration::ClassDecl { .. } => {
                            // ClassDecl is type-level only. No method slots emitted here.
                            // Method name slots come from InstanceDecl entries in this dict.

                            // Named ClassDecl: emit an empty-dict runtime value so the outer
                            // key occupies a slot. This allows leading-dot re-exports like
                            // `Indexable: .Indexable` to reference the class across dict
                            // boundaries. This MUST come AFTER method slots to preserve
                            // slot alignment with the resolver.
                            if se.node.key.is_some() {
                                let key = se.node.key.as_ref().map(|k| {
                                    let lowered = match &k.expr {
                                        // Both plain and annotated VarRef use the name field.
                                        SurfaceExpression::VarRef { name, .. } => {
                                            CoreExpr::Str(name.clone())
                                        }
                                        _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                                    };
                                    Arc::new(Spanned::new(lowered, k.span.clone()))
                                });
                                let value =
                                    Arc::new(Spanned::new(CoreExpr::Dict(vec![]), se.span.clone()));
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
                            // Non-escaped VarRef (bare identifier key) → static string key.
                            // Escaped VarRef ($k:) → computed key, lower as variable lookup.
                            SurfaceExpression::VarRef {
                                name,
                                escaped: false,
                                ..
                            } => CoreExpr::Str(name.clone()),
                            _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                        };
                        Arc::new(Spanned::new(lowered, k.span.clone()))
                    });
                    let value = Arc::new(lower_inner(&se.node.value, diagnostics, scope_frames));
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
            ..
        } => {
            // Compile-time instance dispatch rewriting: if the VarRef node for the function
            // has a call_dispatch annotation set by the type checker, rewrite the function
            // reference to use the resolved VarAddr directly.
            let lowered_func = if let SurfaceExpression::VarRef {
                call_dispatch,
                name,
                ..
            } = &func.expr
            {
                if let Some(addr) = call_dispatch.get() {
                    // The type checker resolved this typeclass method call to a concrete instance
                    // binding and recorded the VarAddr directly. Use it without conversion.
                    Arc::new(Spanned::new(
                        CoreExpr::Var {
                            name: name.clone(),
                            addr: addr.clone(),
                            annotation: None,
                        },
                        func.span.clone(),
                    ))
                } else {
                    Arc::new(lower_inner(func, diagnostics, scope_frames))
                }
            } else {
                Arc::new(lower_inner(func, diagnostics, scope_frames))
            };

            CoreExpr::Call {
                func: lowered_func,
                args: args
                    .iter()
                    .map(|a| Arc::new(lower_inner(a, diagnostics, scope_frames)))
                    .collect(),
                named_args: named_args
                    .iter()
                    .map(|na| {
                        Spanned::new(
                            CoreNamedArg {
                                name: na.node.name.clone(),
                                value: Arc::new(lower_inner(
                                    &na.node.value,
                                    diagnostics,
                                    scope_frames,
                                )),
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
            resolved_captures,
        } => CoreExpr::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    Spanned::new(
                        CoreParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                            slot: i as u32,
                            resolved_type: p.node.resolved_annotation_type.get().and_then(|t| {
                                // Type::Error (failed inference) → None (accept-all).
                                match t {
                                    crate::type_def::Type::Error(_) => None,
                                    _ => Some(t.clone()),
                                }
                            }),
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: Arc::new(lower_inner(body, diagnostics, scope_frames)),
            desugared: *desugared,
            captures: resolved_captures
                .get()
                .expect("resolved_captures not set")
                .clone(),
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
                expr: Arc::new(lower_inner(inner, diagnostics, scope_frames)),
                resolved_type: ty,
                pipeline_blame: None,
            }
        }

        SurfaceExpression::Placeholder(Some(name), _) => CoreExpr::Rest(Some(name.clone())),
        SurfaceExpression::Placeholder(None, _) => CoreExpr::Placeholder,

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower_inner(scrutinee, diagnostics, scope_frames)),
            arms: arms
                .iter()
                .map(|arm| CoreMatchArm {
                    // T-1750: pattern is now Arc<SurfaceNode>, pass through directly (clone the Arc)
                    pattern: Arc::clone(&arm.pattern),
                    guard: arm
                        .guard
                        .as_ref()
                        .map(|g| Arc::new(lower_inner(g, diagnostics, scope_frames))),
                    body: Arc::new(if arm.body.len() == 1 {
                        lower_inner(arm.body_expr(), diagnostics, scope_frames)
                    } else {
                        // Multi-body: wrap in Sequential, same as fn multi-body lowering.
                        // The evaluator evaluates each expression in order and returns the last.
                        Spanned::new(
                            CoreExpr::Sequential(
                                arm.body
                                    .iter()
                                    .map(|e| Arc::new(lower_inner(e, diagnostics, scope_frames)))
                                    .collect(),
                            ),
                            arm.body_expr().span.clone(),
                        )
                    }),
                    guard_matchable_binding: arm.guard_matchable_binding.clone(),
                })
                .collect(),
        },

        SurfaceExpression::Quote(inner) => {
            // Quote captures AST data — VarRefs inside are symbols, not runtime bindings.
            // The resolver intentionally skips Quote bodies, so VarRefs inside will have
            // OnceLock=None. We must not emit "undefined variable" diagnostics for them.
            // scope_frames is passed as None inside Quote: any call_dispatch in a quoted
            // expression is a symbol reference, not a runtime dispatch — coordinates are irrelevant.
            let mut quote_diags = Vec::new();
            CoreExpr::Quote(Arc::new(lower_inner(inner, &mut quote_diags, None)))
        }

        SurfaceExpression::Unquote(inner) => {
            CoreExpr::Unquote(Arc::new(lower_inner(inner, diagnostics, scope_frames)))
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            CoreExpr::UnquoteSplice(Arc::new(lower_inner(inner, diagnostics, scope_frames)))
        }

        SurfaceExpression::PatternDecl { bindings } => CoreExpr::PatternDecl {
            bindings: bindings
                .iter()
                .map(|b| lower_inner(b, diagnostics, scope_frames))
                .collect(),
        },

        SurfaceExpression::LetDecl { bindings } => CoreExpr::LetDecl {
            bindings: bindings
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if i % 2 == 0 {
                        lower_let_decl_binding(b, diagnostics)
                    } else {
                        lower_inner(b, diagnostics, scope_frames)
                    }
                })
                .collect(),
        },

        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => CoreExpr::CaseArm {
            let_bindings: Arc::new(lower_inner(let_bindings, diagnostics, scope_frames)),
            pattern: Arc::new(lower_inner(pattern, diagnostics, scope_frames)),
            body: Arc::new(lower_inner(body, diagnostics, scope_frames)),
        },

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
                                SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                                // Both plain and annotated VarRef use the name field.
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                _ => continue,
                            },
                            None => continue,
                        };

                        let binding_name = crate::type_def::instance_binding_name(
                            &class_decl_name(class_name),
                            &method_name,
                            &type_args,
                        );

                        let key = Some(Arc::new(Spanned::new(
                            CoreExpr::Str(binding_name),
                            syn_span.clone(),
                        )));
                        let value =
                            Arc::new(lower_inner(&me.node.value, diagnostics, scope_frames));
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

        SurfaceExpression::Error(span) => {
            diagnostics.push(LowerDiagnostic {
                kind: LowerDiagnosticKind::Error,
                message: "parse error".to_string(),
                span: span.clone(),
            });
            CoreExpr::Placeholder
        }
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
        CoreExpr::Str(s) => SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: s.clone(),
        },
        CoreExpr::Var {
            name, annotation, ..
        } => SurfaceExpression::VarRef {
            name: name.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: annotation.clone(),
            do_infer_placeholder: false,
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
            pipe_span: None,
        },
        CoreExpr::Fn {
            return_ann,
            params,
            body,
            desugared,
            ..
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
                            resolved_annotation_type: crate::ast::TypeAnnotation::new(),
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: core_expr_to_surface_node(body),
            desugared: *desugared,
            resolved_captures: crate::ast::CapturesCell::new(),
        },
        CoreExpr::TypeAssert {
            annotation, expr, ..
        } => SurfaceExpression::TypeAssert {
            annotation: annotation.clone(),
            expr: core_expr_to_surface_node(expr),
            resolved_type: crate::ast::TypeAnnotation::new(),
        },
        CoreExpr::Rest(name) => SurfaceExpression::Placeholder(name.clone(), None),
        CoreExpr::Match { scrutinee, arms } => SurfaceExpression::Match {
            scrutinee: core_expr_to_surface_node(scrutinee),
            arms: arms
                .iter()
                .map(|arm| SurfaceMatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(|g| core_expr_to_surface_node(g)),
                    body: vec![core_expr_to_surface_node(&arm.body)],
                    guard_matchable_binding: crate::ast::MatchableBinding::new(),
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
        CoreExpr::Placeholder => SurfaceExpression::Placeholder(None, None),
        // Variant: emitted by lower.rs for type declarations; not user-writable in quotes.
        // Represent as a VarRef to the tag so quote round-trips see a name.
        CoreExpr::Variant { tag, .. } => SurfaceExpression::VarRef {
            name: tag.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
            do_infer_placeholder: false,
        },
        CoreExpr::UnitVariant { tycon, ctor } => SurfaceExpression::VarRef {
            name: if tycon.is_empty() {
                ctor.clone()
            } else {
                format!("{}.{}", tycon, ctor)
            },
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
            do_infer_placeholder: false,
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
fn lower_let_decl_binding(
    arc: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<LowerDiagnostic>,
) -> Spanned<CoreExpr> {
    let span = arc.span.clone();
    let core_expr = match &arc.expr {
        // Declaration name forms: lower as string literal (name extraction path)
        // Annotated VarRef (name@Type) is also lowered to Str — the annotation is stripped.
        SurfaceExpression::VarRef { name, .. } => CoreExpr::Str(name.clone()),
        SurfaceExpression::Placeholder(Some(name), _) => CoreExpr::Str(name.clone()),
        // Wildcard / unnamed rest: use empty string (skipped by LetDecl eval arm)
        SurfaceExpression::Placeholder(None, _) => CoreExpr::Str(String::new()),
        // All other forms: lower normally (will produce Error if unresolvable).
        // No scope_frames needed here: LetDecl binding names are not call sites and
        // cannot contain call_dispatch annotations.
        _ => lower_expr(arc, &arc.expr, diagnostics, None),
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
/// Derive a canonical dispatch tag string from any annotation, handling both simple
/// names (`@String` → "String") and complex parametric types (`@[Seq a]` → "[Seq a]").
fn annotation_dispatch_tag(ann: &crate::ast::Annotation) -> Option<String> {
    use crate::ast::Annotation;
    match ann {
        Annotation::Simple(name) => {
            // TypeVars are lowercase — not a dispatch tag.
            if name
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                Some(name.clone())
            } else {
                None
            }
        }
        Annotation::PropertyDict(entries) => {
            // Path 1: normalized simple annotation (@Int → PropertyDict{type: VarRef("Int")}).
            // The parser's normalize_varref_annotation wraps simple names in {type: VarRef(name)}.
            let from_type_key: Option<String> = entries.iter().find_map(|e| {
                let key_str = e.node.key.as_ref().and_then(|k| match &k.expr {
                    SurfaceExpression::StringLiteral { content, .. } => Some(content.as_str()),
                    _ => None,
                });
                if key_str == Some("type") {
                    if let SurfaceExpression::VarRef { name, .. } = &e.node.value.expr {
                        if name
                            .chars()
                            .next()
                            .map(|c| c.is_uppercase())
                            .unwrap_or(false)
                        {
                            Some(name.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
            if from_type_key.is_some() {
                return from_type_key;
            }
            // Path 2: complex parametric annotation (@[Seq a], @[List a], @[Map k v], ...).
            // Build a canonical string from the positional entries. The first entry is the
            // type constructor (uppercase = concrete type). This produces unique tags for
            // distinct parametric types while remaining stable across code edits.
            let parts: Vec<String> = entries
                .iter()
                .map(|e| match &e.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => name.clone(),
                    _ => "_".to_string(),
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(format!("[{}]", parts.join(" ")))
            }
        }
        Annotation::Annotated(base, inner) => {
            // @A@B form — combine both parts.
            let base_tag = annotation_dispatch_tag(base)?;
            let inner_tag = annotation_dispatch_tag(inner).unwrap_or_default();
            Some(if inner_tag.is_empty() {
                base_tag
            } else {
                format!("{}@{}", base_tag, inner_tag)
            })
        }
        Annotation::Quote => None,
    }
}

pub(crate) fn extract_dispatch_tags(arm_pattern: &SurfaceExpression) -> Vec<Option<String>> {
    let bindings = match arm_pattern {
        SurfaceExpression::LetDecl { bindings } => bindings,
        _ => return vec![],
    };
    bindings
        .iter()
        .map(|binding_spanned| match &binding_spanned.expr {
            SurfaceExpression::VarRef {
                annotation: Some(ann),
                ..
            } => annotation_dispatch_tag(&ann.node),
            _ => None,
        })
        .collect()
}

/// Extract the positional parameter count from a class method type signature.
///
/// Class method types follow the pattern `[Fn@RetType [ParamType1 ParamType2 ...]]`.
/// In the surface AST, this is a `Call { func: VarRef("Fn"), args: [Dict([...])] }`.
/// The arity is the number of entries in the parameter list dict.
///
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
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
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
/// - Unit constructors (no annotation) → `CtorName: CoreExpr::UnitVariant { tycon, ctor }`
/// - Unit constructors (with annotation) → `CtorName: [builtin-make-annotated CoreExpr::UnitVariant { tycon, ctor } [key: val ...]]`
/// - Payload constructors → `CtorName: [fn [...fields] CoreExpr::Variant { tag, payload: Some(payload_dict) }]`
///
/// The type name (if present) qualifies the variant tags. When absent, uses unqualified tags.
///
/// Build a `CoreExpr::Dict` representing field-annotations: `{field: {role: "Seq"}, ...}`.
///
/// Returns `None` if field_annotations is empty.
fn build_field_annotations_core_entry(
    field_annotations: &indexmap::IndexMap<String, String>,
    span: &Span,
) -> Option<Spanned<crate::ast::CoreEntry>> {
    use crate::ast::CoreEntry;
    if field_annotations.is_empty() {
        return None;
    }
    // Build inner dicts: for each field, {role: "RoleName"}
    let field_entries: Vec<Spanned<CoreEntry>> = field_annotations
        .iter()
        .map(|(field_name, role)| {
            // Inner dict: {role: "Seq"}
            let role_key = Some(Arc::new(Spanned::new(
                CoreExpr::Str("role".to_string()),
                span.clone(),
            )));
            let role_value = Arc::new(Spanned::new(CoreExpr::Str(role.clone()), span.clone()));
            let role_entry = Spanned::new(
                CoreEntry {
                    key: role_key,
                    value: role_value,
                },
                span.clone(),
            );
            let inner_dict = Arc::new(Spanned::new(CoreExpr::Dict(vec![role_entry]), span.clone()));
            // Outer entry: field_name: {role: "Seq"}
            let key = Some(Arc::new(Spanned::new(
                CoreExpr::Str(field_name.clone()),
                span.clone(),
            )));
            Spanned::new(
                CoreEntry {
                    key,
                    value: inner_dict,
                },
                span.clone(),
            )
        })
        .collect();
    let field_ann_dict = Arc::new(Spanned::new(CoreExpr::Dict(field_entries), span.clone()));
    // Wrap as "field-annotations": {field_name: {role: "..."}, ...}
    let key = Some(Arc::new(Spanned::new(
        CoreExpr::Str("field-annotations".to_string()),
        span.clone(),
    )));
    Some(Spanned::new(
        CoreEntry {
            key,
            value: field_ann_dict,
        },
        span.clone(),
    ))
}

/// Produces `CoreExpr` nodes for each constructor entry in the runtime dict.
fn lower_type_alias_to_constructor_dict(
    type_name_opt: Option<String>,
    body: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<LowerDiagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> CoreExpr {
    use crate::ast::CoreEntry;

    // Extract constructors from the body using the desugar.rs helpers.
    // We need to import the extraction logic. For now, we'll inline a simplified version.
    let ctors = extract_constructors_from_body(&body.expr);

    let syn_span = rust_span!();
    let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::new();

    for ctor in ctors {
        let (tycon, ctor_name) = match &type_name_opt {
            Some(tn) => (tn.clone(), ctor.name.clone()),
            None => (String::new(), ctor.name.clone()),
        };
        let qualified_tag = if tycon.is_empty() {
            ctor_name.clone()
        } else {
            format!("{}.{}", tycon, ctor_name)
        };

        // Create the key for this constructor entry
        let key = Some(Arc::new(Spanned::new(
            CoreExpr::Str(ctor.name.clone()),
            syn_span.clone(),
        )));

        // Create the value: either a unit variant or a constructor function
        let value = if ctor.is_unit {
            // Unit constructor: CoreExpr::UnitVariant { tycon, ctor }
            // If the constructor carries a @[...] annotation (T-1121), wrap with make-annotated.
            let variant_call = Arc::new(Spanned::new(
                CoreExpr::UnitVariant {
                    tycon: tycon.clone(),
                    ctor: ctor_name.clone(),
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
                        let key = se
                            .node
                            .key
                            .as_ref()
                            .map(|k| Arc::new(lower_inner(k, diagnostics, scope_frames)));
                        let value =
                            Arc::new(lower_inner(&se.node.value, diagnostics, scope_frames));
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
                                addr: debruijn_to_var_addr(0, 0),
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
                .enumerate()
                .map(|(i, field_name)| {
                    Spanned::new(
                        crate::ast::CoreParam {
                            name: field_name.clone(),
                            annotation: None,
                            variadic: false,
                            slot: i as u32,
                            resolved_type: None,
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
                            addr: debruijn_to_var_addr(1, idx as u32),
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
                    captures: Arc::new(vec![]),
                },
                syn_span.clone(),
            ));

            // Build annotation dict: combine explicit @[...] entries with field-annotations.
            let has_ann = ctor.annotation.is_some();
            let has_field_anns = !ctor.field_annotations.is_empty();
            if has_ann || has_field_anns {
                let mut ann_core_entries: Vec<Spanned<CoreEntry>> = Vec::new();
                // Include explicit annotation entries.
                if let Some(ann_entries) = &ctor.annotation {
                    for se in ann_entries {
                        let key = se
                            .node
                            .key
                            .as_ref()
                            .map(|k| Arc::new(lower_inner(k, diagnostics, scope_frames)));
                        let value =
                            Arc::new(lower_inner(&se.node.value, diagnostics, scope_frames));
                        ann_core_entries
                            .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                    }
                }
                // Append field-annotations entry if present.
                if let Some(fa_entry) =
                    build_field_annotations_core_entry(&ctor.field_annotations, &syn_span)
                {
                    ann_core_entries.push(fa_entry);
                }
                let ann_dict = Arc::new(Spanned::new(
                    CoreExpr::Dict(ann_core_entries),
                    syn_span.clone(),
                ));
                Arc::new(Spanned::new(
                    CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: "builtin-make-annotated".to_string(),
                                addr: debruijn_to_var_addr(0, 0),
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
    /// Per-field child role annotations from `@Child` on field keys.
    /// Maps field name → role string ("Seq", "One", "MapValues").
    /// Only populated for fields that carry `@Child` annotation.
    field_annotations: indexmap::IndexMap<String, String>,
}

/// Infer the child role from a type expression in a constructor field.
///
/// Examines the surface AST of a field's type expression to determine the
/// structural role for TypeNode's `children`/`map-children` protocol:
///
/// - `[Seq T]` → "Seq" (the field holds a sequence of children)
/// - `[Map K V]` → "MapValues" (the field holds a map whose values are children)
/// - Bare `T` (VarRef) → "One" (the field holds a single child)
/// - Anything else → "One" (default for unrecognized forms)
fn infer_child_role_from_type_expr(expr: &SurfaceExpression) -> &'static str {
    match expr {
        // [Map K V] — Call with Map as head → MapValues role (iterate over values, preserve keys)
        // Any other Call head → One (single child).
        // Note: [Seq T] previously mapped to a "Seq" role, but Seq was eliminated from @Child fields
        // when Intersect.types migrated from [Seq TypeNode] to [Map Int TypeNode]. Only "One" and
        // "MapValues" roles remain.
        SurfaceExpression::Call { func, .. } => match &func.expr {
            SurfaceExpression::VarRef { name, .. } => match name.as_str() {
                "Map" => "MapValues",
                _ => "One",
            },
            _ => "One",
        },
        // Dict with positional entries: [Map K V] parses as Dict([VarRef("Map"), ...])
        SurfaceExpression::Dict(entries) if !entries.is_empty() => {
            if let Some(first) = entries.first() {
                if first.node.key.is_none() {
                    if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                        return match name.as_str() {
                            "Map" => "MapValues",
                            _ => "One",
                        };
                    }
                }
            }
            "One"
        }
        // Bare VarRef → single child
        SurfaceExpression::VarRef { .. } => "One",
        // Anything else → default to single child
        _ => "One",
    }
}

/// Extract constructor information from a TypeAlias body.
///
/// Handles the common constructor forms:
/// 1. Bare VarRef uppercase → unit constructor (e.g., `Red`, `None`)
/// 2. Annotated uppercase → unit constructor with annotation
/// 3. Call with uppercase func + no named args → unit constructor (e.g., `[Ok a]`, `[Error String]`)
/// 4. (legacy) Call with uppercase func + named args → named-field constructor (e.g., `[Circle r: Int]`, old form)
/// 5. (legacy) Dict with first positional VarRef/Annotated + keyed entries → named-field constructor
/// 6. (T-1538 new form) Dict with named uppercase-key entries → payload/unit constructors:
///    `File: [path: String]` (payload), `Noop` (unit)
fn extract_constructors_from_body(body: &SurfaceExpression) -> Vec<ConstructorInfo> {
    let mut ctors = Vec::new();

    fn is_ctor(s: &str) -> bool {
        crate::eval::is_constructor_name(s)
    }

    fn try_extract_one(expr: &SurfaceExpression, ctors: &mut Vec<ConstructorInfo>) {
        match expr {
            // Uppercase VarRef → unit constructor.
            // May carry a PropertyDict annotation (`Red@[category: "primary"]`).
            SurfaceExpression::VarRef {
                name, annotation, ..
            } if is_ctor(name) => {
                let ann_entries = annotation.as_ref().and_then(|ann| match &ann.node {
                    crate::ast::Annotation::PropertyDict(entries) if !entries.is_empty() => {
                        Some(entries.clone())
                    }
                    _ => None,
                });
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    annotation: ann_entries,
                    fields: vec![],
                    field_annotations: indexmap::IndexMap::new(),
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
                        // Literal values (Int/Float/Str/U64/StringLiteral) are compile-time constants.
                        let is_literal = |expr: &SurfaceExpression| {
                            matches!(
                                expr,
                                SurfaceExpression::Int(_)
                                    | SurfaceExpression::U64(_)
                                    | SurfaceExpression::Float(_)
                                    | SurfaceExpression::StringLiteral { .. }
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
                                if let SurfaceExpression::VarRef {
                                    name,
                                    annotation: Some(_),
                                    ..
                                } = &arg.expr
                                {
                                    Some(name.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        let is_unit =
                            payload_named_fields.is_empty() && payload_annotated_fields.is_empty();
                        let fields = if is_unit {
                            vec![]
                        } else {
                            // Collect all payload fields: annotated positional args first,
                            // then named args (non-literal values are runtime fields).
                            let mut all_fields = payload_annotated_fields;
                            all_fields.extend(payload_named_fields);
                            all_fields
                        };
                        // Extract @Child field annotations from named_args and annotated positional args.
                        let mut field_anns = indexmap::IndexMap::new();
                        for na in named_args.iter() {
                            if let Some(ref ann) = na.node.annotation {
                                if matches!(&ann.node, crate::ast::Annotation::Simple(s) if s == "Child")
                                {
                                    let role = infer_child_role_from_type_expr(&na.node.value.expr);
                                    field_anns.insert(na.node.name.clone(), role.to_string());
                                }
                            }
                        }
                        for arg in args.iter() {
                            if let SurfaceExpression::VarRef {
                                name: field_name,
                                annotation: Some(ref ann),
                                ..
                            } = &arg.expr
                            {
                                if matches!(&ann.node, crate::ast::Annotation::Simple(s) if s == "Child")
                                {
                                    // For annotated positional args, the "type expr" is not available
                                    // in this form — default to "One".
                                    field_anns.insert(field_name.clone(), "One".to_string());
                                }
                            }
                        }
                        ctors.push(ConstructorInfo {
                            name: name.clone(),
                            is_unit,
                            annotation: None,
                            fields,
                            field_annotations: field_anns,
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
                    SurfaceExpression::VarRef {
                        name, annotation, ..
                    } if is_ctor(name) => {
                        let ann = annotation.as_ref().and_then(|ann| match &ann.node {
                            crate::ast::Annotation::PropertyDict(entries)
                                if !entries.is_empty() =>
                            {
                                Some(entries.clone())
                            }
                            _ => None,
                        });
                        (name.clone(), ann)
                    }
                    _ => return,
                };
                let is_unit =
                    entries[1..].is_empty() || entries[1..].iter().all(|e| e.node.key.is_none());
                // Collect field names and @Child annotations from keyed entries.
                let mut fields: Vec<String> = Vec::new();
                let mut field_anns = indexmap::IndexMap::new();
                if !is_unit {
                    for e in &entries[1..] {
                        if let Some(k) = &e.node.key {
                            let (field_name, has_child_ann) = match &k.expr {
                                SurfaceExpression::VarRef {
                                    name, annotation, ..
                                } => {
                                    let is_child = annotation.as_ref().is_some_and(|ann| {
                                        matches!(&ann.node, crate::ast::Annotation::Simple(s) if s == "Child")
                                    });
                                    (Some(name.clone()), is_child)
                                }
                                SurfaceExpression::StringLiteral { content, .. } => {
                                    (Some(content.clone()), false)
                                }
                                _ => (None, false),
                            };
                            if let Some(name) = field_name {
                                if has_child_ann {
                                    let role = infer_child_role_from_type_expr(&e.node.value.expr);
                                    field_anns.insert(name.clone(), role.to_string());
                                }
                                fields.push(name);
                            }
                        }
                    }
                }
                ctors.push(ConstructorInfo {
                    name: ctor_name,
                    is_unit,
                    annotation: ctor_annotation,
                    fields,
                    field_annotations: field_anns,
                });
            }
            _ => {}
        }
    }

    // Top-level dispatch: distinguish body forms.
    match body {
        SurfaceExpression::Dict(entries) => {
            // T-1538: New named-key constructor body form.
            // Detected when ANY entry has a named key whose name starts with an uppercase letter.
            // In this form:
            //   - Named entry with uppercase key → payload constructor
            //   - Positional entry with bare uppercase VarRef → unit constructor
            let has_named_uppercase_key = entries.iter().any(|e| {
                if let Some(k) = &e.node.key {
                    matches!(&k.expr, SurfaceExpression::VarRef { name, .. } if is_ctor(name))
                } else {
                    false
                }
            });

            if has_named_uppercase_key {
                // Named-key constructor body: extract from both named and positional entries.
                for entry in entries {
                    if let Some(key_node) = &entry.node.key {
                        // Named entry: uppercase key is constructor name, value is payload dict.
                        if let SurfaceExpression::VarRef {
                            name, annotation, ..
                        } = &key_node.expr
                        {
                            if !is_ctor(name) {
                                continue; // lowercase or non-uppercase key: skip
                            }
                            // Extract constructor-level annotation (e.g., Ctor@[role: "x"]: [...])
                            let ann_entries = annotation.as_ref().and_then(|ann| match &ann.node {
                                crate::ast::Annotation::PropertyDict(ann_entries)
                                    if !ann_entries.is_empty() =>
                                {
                                    Some(ann_entries.clone())
                                }
                                _ => None,
                            });
                            // Collect field names and @Child annotations from the payload dict.
                            let mut fields: Vec<String> = Vec::new();
                            let mut field_anns = indexmap::IndexMap::new();
                            if let SurfaceExpression::Dict(field_entries) = &entry.node.value.expr {
                                for fe in field_entries {
                                    if let Some(k) = &fe.node.key {
                                        let (field_name, has_child_ann) = match &k.expr {
                                            SurfaceExpression::VarRef {
                                                name: fn_,
                                                annotation,
                                                ..
                                            } => {
                                                let is_child = annotation.as_ref().is_some_and(|ann| {
                                                    matches!(&ann.node, crate::ast::Annotation::Simple(s) if s == "Child")
                                                });
                                                (Some(fn_.clone()), is_child)
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => (Some(content.clone()), false),
                                            _ => (None, false),
                                        };
                                        if let Some(fn_) = field_name {
                                            if has_child_ann {
                                                let role = infer_child_role_from_type_expr(
                                                    &fe.node.value.expr,
                                                );
                                                field_anns.insert(fn_.clone(), role.to_string());
                                            }
                                            fields.push(fn_);
                                        }
                                    }
                                }
                            }
                            let is_unit = fields.is_empty();
                            ctors.push(ConstructorInfo {
                                name: name.clone(),
                                is_unit,
                                annotation: ann_entries,
                                fields,
                                field_annotations: field_anns,
                            });
                        }
                    } else {
                        // Positional entry: bare uppercase VarRef → unit constructor.
                        try_extract_one(&entry.node.value.expr, &mut ctors);
                    }
                }
            } else {
                // Old body forms: single-constructor dict or union of positional constructors.
                //
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
        }
        other => try_extract_one(other, &mut ctors),
    }
    ctors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        CallDispatch, Provenance, Resolution, Spanned, SurfaceDeclaration, SurfaceExpression,
        SurfaceItem, SurfaceNode, TypeAnnotation,
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

        let (lowered, diags) = lower(&node, None);

        assert_eq!(lowered.span, span);
        assert!(matches!(lowered.node, CoreExpr::Int(42)));
        assert!(diags.is_empty(), "unexpected diagnostics: {:?}", diags);
    }

    #[test]
    fn test_lower_varref_with_resolution() {
        let span = rust_span!();
        // Build a VarRef node with pre-set inline resolution (LetrecGroupMember { depth: 0, slot: 3 }).
        let resolution = Resolution::new();
        resolution.set(Some(VarAddr::LetrecGroupMember { depth: 0, slot: 3 }));
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "x".into(),
                escaped: false,
                resolution,
                call_dispatch: CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            span,
        );

        let (lowered, diags) = lower(&node, None);

        assert!(diags.is_empty(), "unexpected diagnostics: {:?}", diags);
        match lowered.node {
            CoreExpr::Var { name, addr, .. } => {
                assert_eq!(name, "x");
                // resolution was LetrecGroupMember { depth: 0, slot: 3 } → addr is the same
                assert_eq!(addr, VarAddr::LetrecGroupMember { depth: 0, slot: 3 });
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
                do_infer_placeholder: false,
            },
            span.clone(),
        );

        let (lowered, diags) = lower(&node, None);

        // Unresolvable VarRef produces CoreExpr::Placeholder and a diagnostic.
        assert!(
            matches!(lowered.node, CoreExpr::Placeholder),
            "expected CoreExpr::Placeholder for unresolvable VarRef, got {:?}",
            lowered.node
        );
        // The error must be reported as a diagnostic.
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one diagnostic for unresolvable VarRef"
        );
        assert!(
            matches!(diags[0].kind, LowerDiagnosticKind::Error),
            "expected Error diagnostic, got {:?}",
            diags[0].kind
        );
        assert!(
            diags[0].message.contains("unbound"),
            "diagnostic message should mention the variable name: {}",
            diags[0].message
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
                do_infer_placeholder: false,
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

        let (lowered, _diags) = lower(&node, None);

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
                            do_infer_placeholder: false,
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

        let (lowered, _diags) = lower(&node, None);

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

    // ── builtin-lower Decl-skip key contiguity ────────────────────────────────
    //
    // Regression test for the expr_idx fix (C-516 F-1): when a SurfaceDocument has a
    // SurfaceItem::Decl before a SurfaceItem::Expr, the key for the first Expr item must
    // be "0", not "1". Using enumerate() over all items (before the fix) would assign "1"
    // because the Decl item was counted. The corrected logic uses a separate expr_idx
    // counter incremented only on Expr arms.
    //
    // This test replicates the builtin_lower item-iteration logic directly to verify that
    // the key assigned to the first expression entry in entries[0].0 is "0" regardless
    // of how many Decl items precede it.
    #[test]
    fn test_builtin_lower_decl_skip_key_contiguity() {
        let span = rust_span!();

        // Construct a SurfaceDocument with:
        //   item 0: SurfaceItem::Decl (a TypeAlias declaration — simulates [type Color ...])
        //   item 1: SurfaceItem::Expr (a simple integer literal — simulates [x: 42])
        let decl_body = Arc::new(SurfaceNode::new(SurfaceExpression::Int(0), span.clone()));
        let decl = SurfaceDeclaration::TypeAlias {
            params: vec![],
            body: decl_body,
        };
        let spanned_decl = Spanned::new(decl, span.clone());

        let expr_node = Arc::new(SurfaceNode::new(SurfaceExpression::Int(42), span.clone()));

        let items: Vec<SurfaceItem> = vec![
            SurfaceItem::Decl(spanned_decl),
            SurfaceItem::Expr(expr_node),
        ];

        // Replicate builtin_lower's key-assignment loop.
        let mut entries: Vec<(String, Arc<crate::ast::Spanned<CoreExpr>>)> = Vec::new();
        let mut expr_idx: usize = 0;
        for item in &items {
            let node = match item {
                SurfaceItem::Expr(n) => n,
                SurfaceItem::Decl(_) => continue,
            };
            let (core_spanned, _diags) = lower(node, None);
            entries.push((format!("{expr_idx}"), Arc::new(core_spanned)));
            expr_idx += 1;
        }

        // There should be exactly one entry (the Expr item; the Decl is skipped).
        assert_eq!(
            entries.len(),
            1,
            "expected exactly 1 entry (Decl skipped), got {}",
            entries.len()
        );
        // The key for the first Expr item must be "0", not "1".
        assert_eq!(
            entries[0].0, "0",
            "first expression entry key must be \"0\" after skipping leading Decl items; got {:?}",
            entries[0].0
        );
    }

    // ── process_escapes unit tests ────────────────────────────────────────────
    //
    // These tests pin the escape-processing contract documented in the function's
    // docstring. A mutant that swaps \n→\t or changes unknown-escape behavior
    // would be caught immediately by one of these assertions.

    #[test]
    fn test_process_escapes_newline() {
        assert_eq!(process_escapes(r"\n", "\""), "\n");
    }

    #[test]
    fn test_process_escapes_tab() {
        assert_eq!(process_escapes(r"\t", "\""), "\t");
    }

    #[test]
    fn test_process_escapes_carriage_return() {
        assert_eq!(process_escapes(r"\r", "\""), "\r");
    }

    #[test]
    fn test_process_escapes_backslash() {
        assert_eq!(process_escapes(r"\\", "\""), "\\");
    }

    #[test]
    fn test_process_escapes_delimiter_quote() {
        // \" with delimiter `"` → literal double-quote character
        assert_eq!(process_escapes("\\\"", "\""), "\"");
    }

    #[test]
    fn test_process_escapes_unknown_passthrough() {
        // Unknown escape \x → pass through literally as backslash + 'x' (not an error)
        assert_eq!(process_escapes(r"\x", "\""), "\\x");
    }

    #[test]
    fn test_process_escapes_trailing_backslash() {
        // A trailing backslash with no following char → pass through as backslash
        assert_eq!(process_escapes("\\", "\""), "\\");
    }

    #[test]
    fn test_process_escapes_empty_string() {
        assert_eq!(process_escapes("", "\""), "");
    }

    #[test]
    fn test_process_escapes_mixed() {
        // Full-string test: "say \"hi\" and \\ works" → say "hi" and \ works
        // This mirrors the escape_sequences.llt-eval corpus test.
        assert_eq!(
            process_escapes(r#"say \"hi\" and \\ works"#, "\""),
            r#"say "hi" and \ works"#
        );
    }

    #[test]
    fn test_process_escapes_all_named_escapes_in_sequence() {
        // Verify each named escape in sequence produces the right character.
        // Mutation: swapping \n and \t would fail this test.
        let result = process_escapes("\\n\\t\\r\\\\", "\"");
        assert_eq!(result, "\n\t\r\\");
    }
}
