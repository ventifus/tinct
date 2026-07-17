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

/// Severity of a diagnostic emitted during lowering.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Warning variant is infrastructure for future use
pub enum LowerDiagnosticKind {
    Error,
    Warning,
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
fn resolve_name_in_frames(
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

/// Lower a single surface node to a CoreExpr, collecting diagnostics.
///
/// This is the entry point for per-thunk lowering. Called from `eval_materialize.rs`
/// when a `UnevaluatedState::Surface` thunk is first forced.
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
    let span = arc.span.clone();
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

/// Resolve an annotation name to a Type for TypeAssertPending pattern lowering.
///
/// Mirrors typecheck_annot.rs::resolve_type_name for the builtin type names prelude
/// uses in [@Type _]: patterns. Used when the inline `resolved` TypeAnnotation has no
/// entry (which is always the case currently, as populate is not yet wired up).
/// Unknown is the accept-all fallback for unrecognized names (--no-typecheck, macros).
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
                    // Explicitly marked unresolvable by the resolver — compile error.
                    let message = format!("unresolvable variable: {}", name);
                    diagnostics.push(LowerDiagnostic {
                        kind: LowerDiagnosticKind::Error,
                        message,
                        span: arc.span.clone(),
                    });
                    CoreExpr::Placeholder
                }
                Some(Some((level, slot))) => CoreExpr::Var {
                    name: name.clone(),
                    level,
                    slot,
                    annotation: annotation.clone(),
                },
                None => {
                    if name == "_" {
                        // `_` in pattern position is the wildcard sentinel: always matches,
                        // no binding. The evaluator (eval_structural_pattern_inner) special-cases
                        // Var { name: "_" } → Ok(true). De Bruijn coordinates are never accessed
                        // for the wildcard, so 0/0 are safe dummy values.
                        CoreExpr::Var {
                            name: "_".to_string(),
                            level: 0,
                            slot: 0,
                            annotation: annotation.clone(),
                        }
                    } else {
                        // Resolver ran but this name was not found in any lexical scope.
                        // Every user-written variable reference must have de Bruijn coordinates
                        // assigned by the resolver (seeded from the env). A None here means the
                        // name is genuinely undefined — emit a diagnostic and a Placeholder so
                        // the error surfaces when (and only when) the thunk is forced.
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
        }

        SurfaceExpression::Field {
            expr: Some(inner),
            field,
            field_slot,
            resolution,
        } => {
            // Build the getter function Var and the key argument.
            // The resolver writes (level, slot) for field-get into Field.resolution.
            // field-get and slot-get live in the same env frame; slot-get is always one
            // slot after field-get (by construction in dot-access-env and build_core_env).
            // When resolution is unset, the resolver did not run on this node — emit a
            // diagnostic so the caller fails loudly rather than silently emitting MAX/MAX.
            let (field_get_level, field_get_slot) = match resolution.get() {
                Some(Some((level, slot))) => (level, slot),
                state @ (Some(None) | None) => {
                    let why = if state.is_none() {
                        "resolver did not run on this node"
                    } else {
                        "field-get not found in any scope (resolver ran but returned None)"
                    };
                    diagnostics.push(LowerDiagnostic {
                        kind: LowerDiagnosticKind::Error,
                        message: format!(
                            "field-get: missing resolver coordinates for `.{}` — {}",
                            field, why
                        ),
                        span: arc.span.clone(),
                    });
                    return CoreExpr::Placeholder;
                }
            };

            let (getter_name, getter_level, getter_slot, key_arg) =
                if let Some(typed_slot) = field_slot.get() {
                    // Typed: use slot-get (positional O(1) access).
                    // slot-get is always one slot after field-get in the same env frame.
                    // field_get_slot is always a real slot here: the Some(None) | None arm
                    // above already returned CoreExpr::Placeholder for missing coordinates.
                    (
                        "slot-get",
                        field_get_level,
                        field_get_slot + 1,
                        CoreExpr::Int(typed_slot as i64),
                    )
                } else {
                    // Untyped: use field-get (key-based lookup).
                    let key_core = match field {
                        crate::ast::DotKey::Int(n) => CoreExpr::Int(*n),
                        crate::ast::DotKey::Ident(s) => CoreExpr::Str(s.clone()),
                    };
                    ("field-get", field_get_level, field_get_slot, key_core)
                };

            let getter_var = Arc::new(crate::ast::Spanned::new(
                CoreExpr::Var {
                    name: getter_name.to_string(),
                    level: getter_level,
                    slot: getter_slot,
                    annotation: None,
                },
                arc.span.clone(),
            ));
            let key_node = Arc::new(crate::ast::Spanned::new(key_arg, arc.span.clone()));
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
            Some(Some((level, slot))) => CoreExpr::Var {
                name: name.clone(),
                level,
                slot,
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
        SurfaceExpression::Pipe { lhs, rhs } => CoreExpr::Call {
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
                e.node.key.is_none() && matches!(&e.node.value.expr, SurfaceExpression::Rest(..))
            });
            if has_rest {
                // Collect entry indices between rest markers.
                // segments[i] = indices of regular entries before rest_nodes[i].
                let mut segments: Vec<Vec<usize>> = vec![vec![]];
                let mut rest_indices: Vec<usize> = vec![];
                for (idx, se) in entries.iter().enumerate() {
                    if se.node.key.is_none() {
                        if let SurfaceExpression::Rest(..) = &se.node.value.expr {
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
                                level: u32::MAX,
                                slot: u32::MAX,
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
                                    level: u32::MAX,
                                    slot: u32::MAX,
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

            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            for se in entries {
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                            if se.node.key.is_some() {
                                // Named instance: emit outer key binding only.
                                // Binding names are NOT flattened to avoid duplicate key errors
                                // when multiple instances of the same class exist in the dict.
                                let lowered = lower_expr(
                                    &se.node.value,
                                    &se.node.value.expr,
                                    diagnostics,
                                    scope_frames,
                                );
                                // Named instance keys are always static string keys.
                                // Use the same pattern as regular dict entries: VarRef (plain or
                                // annotated like `FunctorResult@[doc: "..."]`) → Str(name).
                                let key = se.node.key.as_ref().map(|k| {
                                    let key_expr = match &k.expr {
                                        SurfaceExpression::VarRef {
                                            name,
                                            escaped: false,
                                            ..
                                        } => CoreExpr::Str(name.clone()),
                                        _ => lower_expr(k, &k.expr, diagnostics, scope_frames),
                                    };
                                    Arc::new(Spanned::new(key_expr, k.span.clone()))
                                });
                                let value = Arc::new(Spanned::new(lowered, se.span.clone()));
                                core_entries
                                    .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                            } else {
                                // Anonymous instance: flatten binding names into the outer dict.
                                // surface_dict_static_keys emits the same names so the resolver's
                                // letrec scope matches this layout exactly.
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
                                                // Both plain and annotated VarRef use the name field.
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    name.clone()
                                                }
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
                        crate::ast::SurfaceDeclaration::ClassDecl {
                            name: _class_name,
                            methods,
                            ..
                        } => {
                            // Named ClassDecl: emit an empty-dict runtime value so the outer
                            // key occupies a slot. This allows leading-dot re-exports like
                            // `Indexable: .Indexable` to reference the class across dict
                            // boundaries.
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

                            // Class methods contribute nothing to the eval env.
                            // Dispatch is resolved statically by the type checker (call_dispatch
                            // annotation on the call-site VarRef). If the type checker can't
                            // resolve the instance, the call falls through to whatever function
                            // is in scope under that name — or becomes an undefined-variable error.
                            // The only method with a deliberate eval-env fallback is `=` (returns
                            // Boolean.False for unknown types), defined explicitly in prelude.llt.
                            let _ = methods; // suppress unused warning
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
        } => {
            // Compile-time instance dispatch rewriting: if the VarRef node for the function
            // has a call_dispatch annotation set by the type checker, rewrite the function
            // reference to the instance binding name.
            let lowered_func = if let SurfaceExpression::VarRef { call_dispatch, .. } = &func.expr {
                if let Some(mangled_name) = call_dispatch.get() {
                    // The type checker resolved this typeclass method call to a concrete instance
                    // binding. Resolve the mangled name to De Bruijn coordinates using the
                    // accumulated scope frames from the resolver run.
                    match scope_frames.and_then(|f| resolve_name_in_frames(f, &mangled_name)) {
                        Some((level, slot)) => Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: mangled_name.to_string(),
                                level,
                                slot,
                                annotation: None,
                            },
                            func.span.clone(),
                        )),
                        None => {
                            diagnostics.push(LowerDiagnostic {
                                kind: LowerDiagnosticKind::Error,
                                message: format!(
                                    "call_dispatch: scope frames not available or instance binding '{}' not found — resolver did not run on this node",
                                    mangled_name
                                ),
                                span: func.span.clone(),
                            });
                            Arc::new(Spanned::new(CoreExpr::Placeholder, func.span.clone()))
                        }
                    }
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
            body: Arc::new(lower_inner(body, diagnostics, scope_frames)),
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
                expr: Arc::new(lower_inner(inner, diagnostics, scope_frames)),
                resolved_type: ty,
                pipeline_blame: None,
            }
        }

        SurfaceExpression::Rest(name, _) => CoreExpr::Rest(name.clone()),

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower_inner(scrutinee, diagnostics, scope_frames)),
            arms: arms
                .iter()
                .map(|arm| CoreMatchArm {
                    pattern: Spanned::new(
                        lower_pattern(&arm.pattern.node),
                        arm.pattern.span.clone(),
                    ),
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
                                SurfaceExpression::StringLiteral { content, .. } => content.clone(),
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
        SurfaceExpression::Rest(Some(name), _) => CoreExpr::Str(name.clone()),
        // Wildcard / unnamed rest: use empty string (skipped by LetDecl eval arm)
        SurfaceExpression::Rest(None, _) => CoreExpr::Str(String::new()),
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
                SurfaceExpression::VarRef {
                    annotation: Some(ann),
                    ..
                } => {
                    // Extract the type name from annotations. VarRef annotations are normalized
                    // from Simple("T") to PropertyDict{type: VarRef("T")} at parse time, so we
                    // handle both forms. Only uppercase type names are valid dispatch tags.
                    use crate::ast::Annotation;
                    let type_name_opt = match &ann.node {
                        Annotation::Simple(type_name) => Some(type_name.as_str()),
                        Annotation::PropertyDict(entries) => {
                            // Find the "type" key entry and extract its VarRef name.
                            entries.iter().find_map(|e| {
                                let key_str = e.node.key.as_ref().and_then(|k| match &k.expr {
                                    SurfaceExpression::StringLiteral { content, .. } => {
                                        Some(content.as_str())
                                    }
                                    _ => None,
                                });
                                if key_str == Some("type") {
                                    if let SurfaceExpression::VarRef { name, .. } =
                                        &e.node.value.expr
                                    {
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

/// Extract the positional parameter count from a class method type signature.
///
/// Class method types follow the pattern `[Fn@RetType [ParamType1 ParamType2 ...]]`.
/// In the surface AST, this is a `Call { func: VarRef("Fn"), args: [Dict([...])] }`.
/// The arity is the number of entries in the parameter list dict.

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
                // Collect field names from keyed entries for named-field constructors.
                let fields: Vec<String> = if is_unit {
                    vec![]
                } else {
                    entries[1..]
                        .iter()
                        .filter_map(|e| {
                            e.node.key.as_ref().and_then(|k| match &k.expr {
                                SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                                SurfaceExpression::StringLiteral { content, .. } => {
                                    Some(content.clone())
                                }
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
                            // Collect field names from the payload dict.
                            let fields: Vec<String> = match &entry.node.value.expr {
                                SurfaceExpression::Dict(field_entries) => field_entries
                                    .iter()
                                    .filter_map(|fe| {
                                        fe.node.key.as_ref().and_then(|k| match &k.expr {
                                            SurfaceExpression::VarRef { name: fn_, .. } => {
                                                Some(fn_.clone())
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => Some(content.clone()),
                                            _ => None,
                                        })
                                    })
                                    .collect(),
                                _ => vec![],
                            };
                            let is_unit = fields.is_empty();
                            ctors.push(ConstructorInfo {
                                name: name.clone(),
                                is_unit,
                                annotation: ann_entries,
                                fields,
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

        let (lowered, diags) = lower(&node, None);

        assert_eq!(lowered.span, span);
        assert!(matches!(lowered.node, CoreExpr::Int(42)));
        assert!(diags.is_empty(), "unexpected diagnostics: {:?}", diags);
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

        let (lowered, diags) = lower(&node, None);

        assert!(diags.is_empty(), "unexpected diagnostics: {:?}", diags);
        match lowered.node {
            CoreExpr::Var {
                name, level, slot, ..
            } => {
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
