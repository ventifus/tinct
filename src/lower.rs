//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `SurfaceNode` — all cross-phase data lives inline on nodes.
//! De Bruijn coordinates are read from the inline `resolution` field on VarRef/Field nodes.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `Placeholder` (unresolvable — diagnostic emitted)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (TypeAssertCheck::Resolved when type checker ran, Source otherwise)
//! - `Field { expr: Some(inner) }` → `CoreExpr::Field { target, key }` (structural field access)
//! - `UnitVariant` → `CoreExpr::UnitVariant` (1:1 structural lowering)
//! - `MakeVariant` → `CoreExpr::Variant` (1:1 structural lowering)
//! - `TypeDecl` → `CoreExpr::TypeDecl` (1:1 structural lowering)
//! - `ReprDecl` → `CoreExpr::ReprDecl` (1:1 structural lowering)
//! - `SurfaceNode.type_guard` set → wraps the lowered CoreExpr in `CoreExpr::TypeAssert`
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::Arc;

use crate::ast::{
    CoreClause, CoreEntry, CoreExpr, CoreNamedArg, CoreParam, Spanned, SurfaceExpression,
    SurfaceNode, VarAddr,
};
use crate::error::Diagnostic;
use crate::eval_call::is_typevalue_unknown;
use crate::rust_span;
use crate::type_infer::{
    make_typevalue_fn_with_flags, make_typevalue_nominal, make_typevalue_unknown,
};

const KIND: &str = "lower";
const KIND_TRACE: &str = "trace";

/// Name of the raise builtin — used in tests to verify consistency with builtins_core.rs
/// registration. The MethodDispatcher was removed in S-1024; EffectPerform now handles dispatch.
/// Part of the Rust-tinct protocol (doc/16b §7).
///
/// The `builtin!` macro in `src/builtins_core.rs` requires a literal (`$name:literal`)
/// and cannot accept this constant directly. The registration literal `"builtin-raise"` in
/// `builtins_core.rs` must match this constant. A test verifies their consistency:
/// `test_builtin_raise_name_registered_in_core_builtins` in the test module below.
#[cfg(test)]
pub(crate) const BUILTIN_RAISE_NAME: &str = "builtin-raise";

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
/// This is the entry point for per-thunk lowering. Called eagerly during `builtin-lower`
/// (the discrete lowering pipeline step) and from other callers that need a CoreExpr.
///
/// Lowering errors (unresolvable variables, malformed AST) are reported as `Diagnostic`
/// entries in the returned vec and the corresponding expression is replaced with
/// `CoreExpr::Placeholder`. Callers that need eager error reporting (e.g., document loading)
/// inspect the diagnostic vec; callers that discard it accept that the placeholder will error
/// at runtime if forced.
///
/// All cross-phase data (type annotations, field slots, provenance) is read from inline
/// fields on the AST nodes — no external tables are consulted.
pub fn lower(
    arc: &Arc<SurfaceNode>,
    type_display_names: &std::collections::HashMap<u64, String>,
) -> crate::error::DiagnosticResult<Spanned<CoreExpr>> {
    let mut diagnostics = Vec::new();
    let spanned = lower_inner(arc, &mut diagnostics, type_display_names);
    crate::error::DiagnosticResult::ok(spanned).with_diagnostics(diagnostics)
}

/// Internal lowering entry point that threads the diagnostics accumulator.
///
/// Used by recursive calls within lower.rs and by eval machinery that does not need the
/// diagnostic Vec. When a VarRef or parse error is encountered, a `Diagnostic` is
/// pushed and `CoreExpr::Placeholder` is emitted. Produces the same `Spanned<CoreExpr>`
/// as the public `lower()`.
pub(crate) fn lower_inner(
    arc: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<Diagnostic>,
    type_display_names: &std::collections::HashMap<u64, String>,
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

    // Read the inferred type from the SurfaceNode BEFORE lowering, so lower_expr
    // can use it directly during CoreExpr construction — no temporary Unknown.
    let inferred_type = arc
        .inferred_type_decl_id
        .get()
        .cloned()
        .unwrap_or_else(make_typevalue_unknown);

    let mut core_expr = lower_expr(arc, &arc.expr, diagnostics, type_display_names);
    core_expr.set_inferred_type(Arc::clone(&inferred_type));

    // Apply type guard if the type checker set one on this node.
    let core_expr = if let Some(guard_type) = arc.type_guard.get() {
        CoreExpr::TypeAssert {
            expr: Arc::new(crate::ast::Spanned::new(core_expr, span.clone())),
            check: crate::ast::TypeAssertCheck::Resolved(guard_type.clone()),
            pipeline_blame: None,
            inferred_type,
        }
    } else {
        core_expr
    };

    Spanned::new(core_expr, span)
}

// lower_pattern deleted — match arm patterns are now Arc<SurfaceNode>, passed through unchanged.

fn lower_expr(
    arc: &Arc<SurfaceNode>,
    expr: &SurfaceExpression,
    diagnostics: &mut Vec<Diagnostic>,
    type_display_names: &std::collections::HashMap<u64, String>,
) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int {
            value: *n,
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_INT),
        },
        SurfaceExpression::U64(n) => CoreExpr::U64 {
            value: *n,
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_U64),
        },
        SurfaceExpression::Float(n) => CoreExpr::Float {
            value: *n,
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_FLOAT),
        },
        SurfaceExpression::StringLiteral {
            prefix,
            delimiter,
            content,
        } => {
            assert!(
                prefix.is_empty(),
                "i-strings (prefix='{}') must be desugared before lowering reaches StringLiteral",
                prefix
            );
            if delimiter.len() == 1 {
                // Single-quoted: process escape sequences
                CoreExpr::Str {
                    value: process_escapes(content, delimiter),
                    inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
                }
            } else {
                // Triple-quoted (or longer): no escape processing
                // Content is already raw — pass through as-is
                CoreExpr::Str {
                    value: content.clone(),
                    inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
                }
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
                    // intentionally suppressed it in pattern position).
                    //
                    // `_` is the conventional wildcard name: emit Placeholder so it
                    // continues to act as an unconditional wildcard in pattern position
                    // (same semantics as `...`). This is a language-level convention —
                    // `_` means "don't care" everywhere patterns appear.
                    //
                    // All other undefined names emit a sentinel Var. Placeholder (`...`)
                    // is the unconditional wildcard; an undefined VarRef for any other
                    // name must NOT act as a wildcard in pattern position, because
                    // `x: body` where `x` is out of scope should produce a non-matching
                    // pin (MatchExhaustion), not a match-everything arm.
                    //
                    // The sentinel addr Dispatch(0, u32::MAX) is never assigned by
                    // the resolver to real bindings (slot indices are assigned 0..N-1).
                    // In pattern position, bind_or_pin_name detects this sentinel and
                    // returns Ok(false) (non-match). In body position, eval_core's Var
                    // arm looks up group[u32::MAX as usize] → None →
                    // Diagnostic (undefined variable), which is the correct deferred error
                    // for referencing an undefined name.
                    if name == "_" {
                        CoreExpr::Placeholder {
                            inferred_type: make_typevalue_unknown(),
                        }
                    } else {
                        CoreExpr::Var {
                            name: name.clone(),
                            addr: VarAddr::Dispatch(0, u32::MAX),
                            annotation: annotation.clone(),
                            inferred_type: make_typevalue_unknown(),
                        }
                    }
                }
                Some(Some(addr)) => CoreExpr::Var {
                    name: name.clone(),
                    addr: addr.clone(),
                    annotation: annotation.clone(),
                    inferred_type: make_typevalue_unknown(),
                },
                None => {
                    let message = format!("VarRef: \"{}\" not resolved", name);
                    diagnostics.push(Diagnostic::error(KIND, message, arc.span.clone()));
                    CoreExpr::Placeholder {
                        inferred_type: make_typevalue_unknown(),
                    }
                }
            }
        }

        SurfaceExpression::Field {
            expr: Some(inner),
            field,
            ..
        } => CoreExpr::Field {
            target: Arc::new(lower_inner(inner, diagnostics, type_display_names)),
            key: field.clone(),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::Field {
            expr: None,
            field,
            resolution,
            ..
        } => match resolution.get() {
            Some(Some(addr)) => CoreExpr::Var {
                name: field.to_string(),
                addr: addr.clone(),
                annotation: None,
                inferred_type: make_typevalue_unknown(),
            },
            _ => {
                diagnostics.push(Diagnostic::error(
                    KIND,
                    format!("Field: \"{}\" not resolved", field),
                    arc.span.clone(),
                ));
                CoreExpr::Placeholder {
                    inferred_type: make_typevalue_unknown(),
                }
            }
        },

        // Pipe is syntactic sugar — rewrite to Call(rhs, [lhs]) so the evaluator
        // sees only Call nodes. Equivalent to: f |> g  ==  g(f).
        SurfaceExpression::Pipe { lhs, rhs, .. } => CoreExpr::Call {
            func: Arc::new(lower_inner(rhs, diagnostics, type_display_names)),
            args: vec![Arc::new(lower_inner(lhs, diagnostics, type_display_names))],
            named_args: vec![],
            implied: true,
            inferred_type: make_typevalue_unknown(),
            type_instantiation: None,
        },

        SurfaceExpression::Sequential(exprs) => CoreExpr::Sequential {
            exprs: exprs
                .iter()
                .map(|e| Arc::new(lower_inner(e, diagnostics, type_display_names)))
                .collect(),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::Dict { entries, ref scope } => {
            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            for se in entries {
                let key = se.node.key.as_ref().map(|k| {
                    let lowered = match &k.expr {
                        // Non-escaped VarRef (bare identifier key) → static string key.
                        // Escaped VarRef ($k:) → computed key, lower as variable lookup.
                        SurfaceExpression::VarRef {
                            name,
                            escaped: false,
                            ..
                        } => CoreExpr::Str {
                            value: name.clone(),
                            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
                        },
                        _ => lower_expr(k, &k.expr, diagnostics, type_display_names),
                    };
                    Arc::new(Spanned::new(lowered, k.span.clone()))
                });
                let value = Arc::new(lower_inner(&se.node.value, diagnostics, type_display_names));
                // Trivial self-reference: [name: name] where the entire RHS is a bare
                // VarRef resolving to the same letrec binding. depth=0 means current
                // group; name equality guarantees same slot (names are unique per group).
                // Anything inside fn or call may be valid recursion — only the bare case
                // is detectable as definitely wrong.
                // Key representation: bare word keys are StringLiteral (normalized at parse
                // time); annotated keys (`name@[doc: "..."]`) stay as VarRef. Both must be
                // matched. Look through TypeAssert/annotation wrappers on the value too.
                if let Some(key_node) = &se.node.key {
                    let key_name: Option<&str> = match &key_node.expr {
                        SurfaceExpression::StringLiteral { content, .. } => Some(content.as_str()),
                        SurfaceExpression::VarRef {
                            name,
                            escaped: false,
                            ..
                        } => Some(name.as_str()),
                        _ => None,
                    };
                    if let Some(key_name) = key_name {
                        // Peel TypeAssert wrappers added by the type checker or user
                        // annotations — the bare Var may be wrapped in one or more guards.
                        let mut inner = &value.node;
                        while let CoreExpr::TypeAssert { expr, .. } = inner {
                            inner = &expr.node;
                        }
                        if let CoreExpr::Var {
                            name: var_name,
                            addr: VarAddr::Dispatch(0, _),
                            ..
                        } = inner
                        {
                            if var_name.as_str() == key_name {
                                diagnostics.push(Diagnostic::error(
                                        KIND,
                                        format!(
                                            "self-referential binding: '{}' refers to itself; use '.{}' to reference the parent scope",
                                            key_name, key_name
                                        ),
                                        se.node.value.span.clone(),
                                    ));
                            }
                        }
                    }
                }
                core_entries.push(Spanned::new(
                    CoreEntry {
                        key,
                        value,
                        assigned_slot: se
                            .node
                            .assigned_slot
                            .get()
                            .copied()
                            .expect("resolver must set assigned_slot"),
                    },
                    se.span.clone(),
                ));
            }
            CoreExpr::Dict {
                entries: core_entries,
                scope: scope.get().cloned(),
                inferred_type: make_typevalue_unknown(),
            }
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
            ..
        } => CoreExpr::Call {
            func: Arc::new(lower_inner(func, diagnostics, type_display_names)),
            args: args
                .iter()
                .map(|a| Arc::new(lower_inner(a, diagnostics, type_display_names)))
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
                                type_display_names,
                            )),
                        },
                        na.span.clone(),
                    )
                })
                .collect(),
            implied: *implied,
            inferred_type: make_typevalue_unknown(),
            type_instantiation: arc.type_instantiation.get().cloned(),
        },

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
            resolved_return_annotation,
            ..
        } => {
            // Lower the body first to get its span for wrapping.
            let lowered_body_spanned = lower_inner(body, diagnostics, type_display_names);
            let body_span = lowered_body_spanned.span.clone();

            // Build params and collect TypeAssert checks for each typed parameter.
            // For each param with a concrete resolved TypeValue, emit a TypeAssert at the
            // beginning of the function body so the check fires on every invocation.
            let mut param_assert_exprs: Vec<Arc<crate::ast::Spanned<CoreExpr>>> = Vec::new();
            let params_built: Vec<crate::ast::Spanned<crate::ast::CoreParam>> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let resolved = p.node.resolved_annotation_type.get().cloned();
                    // Emit a TypeAssert check for this param if the type checker resolved it to a
                    // concrete (non-Unknown) TypeValue. Unknown passes all checks — no point emitting.
                    if let Some(ref tv) = resolved {
                        if !is_typevalue_unknown(tv) {
                            let slot = p.node.resolved_dispatch_slot.get().unwrap_or(i as u32);
                            let param_ref = CoreExpr::Var {
                                name: p.node.name.clone(),
                                addr: VarAddr::Dispatch(0, slot),
                                annotation: None,
                                inferred_type: make_typevalue_unknown(),
                            };
                            let assert = CoreExpr::TypeAssert {
                                expr: Arc::new(crate::ast::Spanned::new(param_ref, p.span.clone())),
                                check: crate::ast::TypeAssertCheck::Resolved(tv.clone()),
                                pipeline_blame: None,
                                inferred_type: make_typevalue_unknown(),
                            };
                            param_assert_exprs
                                .push(Arc::new(crate::ast::Spanned::new(assert, p.span.clone())));
                        }
                    }
                    Spanned::new(
                        CoreParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                            slot: i as u32,
                            // TypeAnnotation::get() now returns Option<&Arc<Value>>.
                            // TypeValue.Unknown (which Error maps to) is treated as accept-all at
                            // runtime — pass it through as-is; None means unannotated.
                            resolved_type: p.node.resolved_annotation_type.get().cloned(),
                        },
                        p.span.clone(),
                    )
                })
                .collect();

            // Prepend param TypeAsserts before the body. If there are no typed params the body
            // is used directly. If there are typed params, wrap in a Sequential so the checks
            // execute before the body is forced.
            let body_with_param_checks = if param_assert_exprs.is_empty() {
                lowered_body_spanned
            } else {
                param_assert_exprs.push(Arc::new(lowered_body_spanned));
                crate::ast::Spanned::new(
                    CoreExpr::Sequential {
                        exprs: param_assert_exprs,
                        inferred_type: make_typevalue_unknown(),
                    },
                    body_span.clone(),
                )
            };

            // Wrap body with TypeAssert for the return type annotation.
            // Use TypeAssertCheck::Resolved when the type checker has populated the resolved type.
            // Fall back to Source (runtime annotation evaluation) when the type checker did not run.
            let final_body = if let Some(ann) = return_ann {
                let check = if let Some(tv) = resolved_return_annotation.get().cloned() {
                    if !is_typevalue_unknown(&tv) {
                        crate::ast::TypeAssertCheck::Resolved(tv)
                    } else {
                        crate::ast::TypeAssertCheck::Source {
                            annotation: ann.clone(),
                        }
                    }
                } else {
                    crate::ast::TypeAssertCheck::Source {
                        annotation: ann.clone(),
                    }
                };
                crate::ast::Spanned::new(
                    CoreExpr::TypeAssert {
                        expr: Arc::new(body_with_param_checks),
                        check,
                        pipeline_blame: None,
                        inferred_type: make_typevalue_unknown(),
                    },
                    body_span,
                )
            } else {
                body_with_param_checks
            };

            // Build the resolved TypeValue.Fn so the runtime can store it in Thunk.type_decl_id
            // for full structural type checking via value_matches_type.
            // Use Unknown for unannotated params/return (gradual: accepts anything).
            let resolved_fn_type: Option<std::sync::Arc<crate::value::Value>> = {
                let ret_tv = resolved_return_annotation
                    .get()
                    .map(Arc::clone)
                    .filter(|tv| !is_typevalue_unknown(tv))
                    .unwrap_or_else(make_typevalue_unknown);
                let fn_params: Vec<(Option<String>, std::sync::Arc<crate::value::Value>)> = params
                    .iter()
                    .filter(|p| !p.node.variadic)
                    .map(|p| {
                        let tv = p
                            .node
                            .resolved_annotation_type
                            .get()
                            .map(Arc::clone)
                            .unwrap_or_else(make_typevalue_unknown);
                        (Some(p.node.name.clone()), tv)
                    })
                    .collect();
                let is_variadic = params.iter().any(|p| p.node.variadic);
                // Count required params (those without default: annotation). None = all required.
                let required_count = {
                    let mut non_variadic = params.iter().filter(|p| !p.node.variadic);
                    let count = params.iter().filter(|p| !p.node.variadic).count();
                    let has_defaults = non_variadic.any(|p| {
                        p.node.annotation.as_ref().is_some_and(|ann| {
                            ann.node
                                .get_property(crate::ast::ANNOTATION_KEY_DEFAULT)
                                .is_some()
                        })
                    });
                    if has_defaults {
                        let req = params
                            .iter()
                            .filter(|p| {
                                !p.node.variadic
                                    && p.node.annotation.as_ref().is_none_or(|ann| {
                                        ann.node
                                            .get_property(crate::ast::ANNOTATION_KEY_DEFAULT)
                                            .is_none()
                                    })
                            })
                            .count();
                        Some(req)
                    } else if count > 0 {
                        None // All params required
                    } else {
                        Some(0) // No fixed params
                    }
                };
                Some(make_typevalue_fn_with_flags(
                    fn_params,
                    ret_tv,
                    required_count,
                    is_variadic,
                    Vec::new(),
                ))
            };

            // Emit trace diagnostic if return_ann has trace: property with value >= 1.
            // Value-based guard matches typecheck and runtime phases: trace: 0 does not fire.
            let trace_level: u32 = return_ann
                .as_ref()
                .and_then(|ann| ann.node.get_property(crate::ast::ANNOTATION_KEY_TRACE))
                .and_then(|node| {
                    if let crate::ast::SurfaceExpression::Int(n) = &node.expr {
                        Some(*n as u32)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if trace_level >= 1 {
                let sig = resolved_fn_type
                    .as_ref()
                    .map(|tv| crate::eval::format_type_with_names(tv, type_display_names))
                    .unwrap_or_else(|| "<no type>".to_string());
                diagnostics.push(Diagnostic::info(KIND_TRACE, sig, arc.span.clone()));
            }

            let clause = crate::ast::CoreClause {
                params: params_built,
                lowered_pattern: None,
                guard: None,
                body: Arc::new(final_body),
                guard_matchable_binding: crate::ast::MatchableBinding::new(),
            };
            CoreExpr::Fn {
                clauses: vec![clause],
                desugared: *desugared,
                resolved_fn_type,
                // Carry return_ann into CoreExpr::Fn so the evaluator can populate
                // FnAnnotation.extra at function-definition time.
                // This enables annotation-driven runtime behavior: trace:, as-type:, etc.
                return_ann: return_ann.clone(),
                inferred_type: make_typevalue_unknown(),
            }
        }

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => {
            // Select the appropriate TypeAssertCheck variant:
            // - If the type checker ran and produced a concrete TypeValue → Resolved(tv)
            //   (skips runtime annotation parsing; check is a pre-resolved Arc<Value>)
            // - If type checker did not run (resolved_type OnceLock not set) → Source { annotation }
            //   (runtime evaluates the annotation to determine the type to check against)
            let check = match resolved_type.get().cloned() {
                Some(tv) if !is_typevalue_unknown(&tv) => crate::ast::TypeAssertCheck::Resolved(tv),
                _ => crate::ast::TypeAssertCheck::Source {
                    annotation: annotation.clone(),
                },
            };
            CoreExpr::TypeAssert {
                expr: Arc::new(lower_inner(inner, diagnostics, type_display_names)),
                check,
                pipeline_blame: None,
                inferred_type: make_typevalue_unknown(),
            }
        }

        SurfaceExpression::Placeholder(Some(name), _) => CoreExpr::Rest {
            name: Some(name.clone()),
            inferred_type: make_typevalue_unknown(),
        },
        SurfaceExpression::Placeholder(None, _) => CoreExpr::Placeholder {
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower_inner(scrutinee, diagnostics, type_display_names)),
            inferred_type: make_typevalue_unknown(),
            arms: arms
                .iter()
                .map(|arm| {
                    // Build params from let_bindings (T-2128).
                    // Extract binding names from [let name1 name2 ...], mirroring
                    // resolve.rs::extract_case_arm_binding_names. The resolver assigns
                    // Dispatch(0, i) to each name via per-frame relative slots;
                    // lowering materializes that as CoreParam entries (slot = i, variadic = false).
                    // `_` is excluded — it is a wildcard, not a binding.
                    let let_bindings = &arm.let_bindings;
                    let params: Vec<crate::ast::Spanned<CoreParam>> = {
                        let names: Vec<String> = match &let_bindings.expr {
                            SurfaceExpression::LetDecl { bindings } => bindings
                                .iter()
                                .filter_map(|b| {
                                    if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                                        if name == "_" {
                                            None
                                        } else {
                                            Some(name.clone())
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        names
                            .into_iter()
                            .enumerate()
                            .map(|(i, name)| {
                                Spanned::new(
                                    CoreParam {
                                        name,
                                        slot: i as u32,
                                        variadic: false,
                                        annotation: None,
                                        resolved_type: None,
                                    },
                                    let_bindings.span.clone(),
                                )
                            })
                            .collect()
                    };
                    // Lower the arm pattern for all match arms.
                    let lowered_pattern = Some(Arc::new(lower_inner(
                        &arm.pattern,
                        diagnostics,
                        type_display_names,
                    )));
                    CoreClause {
                        params,
                        lowered_pattern,
                        guard: arm
                            .guard
                            .as_ref()
                            .map(|g| Arc::new(lower_inner(g, diagnostics, type_display_names))),
                        // body is always a single node (parser wraps multi-body in Sequential).
                        body: Arc::new(lower_inner(
                            arm.body_expr(),
                            diagnostics,
                            type_display_names,
                        )),
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                    }
                })
                .collect(),
        },

        SurfaceExpression::Quote(inner) => {
            // Quote captures AST data — VarRefs inside are symbols, not runtime bindings.
            // The resolver intentionally skips Quote bodies, so VarRefs inside will have
            // OnceLock=None. We must not emit "undefined variable" diagnostics for them.
            let mut quote_diags = Vec::new();
            CoreExpr::Quote {
                expr: Arc::new(lower_inner(inner, &mut quote_diags, type_display_names)),
                inferred_type: make_typevalue_unknown(),
            }
        }

        SurfaceExpression::Unquote(inner) => CoreExpr::Unquote {
            expr: Arc::new(lower_inner(inner, diagnostics, type_display_names)),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::UnquoteSplice(inner) => CoreExpr::UnquoteSplice {
            expr: Arc::new(lower_inner(inner, diagnostics, type_display_names)),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::PatternDecl { bindings } => CoreExpr::PatternDecl {
            bindings: bindings
                .iter()
                .map(|b| lower_inner(b, diagnostics, type_display_names))
                .collect(),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::LetDecl { bindings } => CoreExpr::LetDecl {
            bindings: bindings
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if i % 2 == 0 {
                        lower_let_decl_binding(b, diagnostics, type_display_names)
                    } else {
                        lower_inner(b, diagnostics, type_display_names)
                    }
                })
                .collect(),
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::Decl(_) => CoreExpr::Placeholder {
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::UnitVariant {
            ctor_index,
            type_decl_id,
        } => CoreExpr::UnitVariant {
            ctor_index: *ctor_index,
            type_decl_id: *type_decl_id,
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::MakeVariant {
            ctor_index,
            payload,
            type_decl_id,
        } => CoreExpr::Variant {
            ctor_index: *ctor_index,
            payload: Some(Arc::new(lower_inner(
                payload,
                diagnostics,
                type_display_names,
            ))),
            type_decl_id: *type_decl_id,
            inferred_type: make_typevalue_unknown(),
        },

        SurfaceExpression::TypeDecl {
            type_name,
            type_decl_id,
            params: _,
            inner,
        } => {
            let inner_lowered = Arc::new(lower_inner(inner, diagnostics, type_display_names));
            let inferred = inner
                .inferred_type_decl_id
                .get()
                .cloned()
                .unwrap_or_else(make_typevalue_unknown);
            CoreExpr::TypeDecl {
                type_name: type_name.clone(),
                type_decl_id: *type_decl_id,
                inner: inner_lowered,
                inferred_type: inferred,
            }
        }

        SurfaceExpression::ReprDecl {
            repr,
            is_pred,
            inner,
        } => {
            let inferred = inner
                .inferred_type_decl_id
                .get()
                .cloned()
                .unwrap_or_else(make_typevalue_unknown);
            CoreExpr::ReprDecl {
                repr: *repr,
                is_pred: is_pred
                    .as_ref()
                    .map(|pred| Arc::new(lower_inner(pred, diagnostics, type_display_names))),
                inner: Arc::new(lower_inner(inner, diagnostics, type_display_names)),
                inferred_type: inferred,
            }
        }

        SurfaceExpression::Error(span) => {
            diagnostics.push(Diagnostic::error(
                KIND,
                "parse error".to_string(),
                span.clone(),
            ));
            CoreExpr::Placeholder {
                inferred_type: make_typevalue_unknown(),
            }
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
        CoreExpr::Int { value: n, .. } => SurfaceExpression::Int(*n),
        CoreExpr::U64 { value: n, .. } => SurfaceExpression::U64(*n),
        CoreExpr::Float { value: f, .. } => SurfaceExpression::Float(*f),
        CoreExpr::Str { value: s, .. } => SurfaceExpression::StringLiteral {
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

            annotation: annotation.clone(),
            do_infer_placeholder: false,
        },
        CoreExpr::Field { target, key, .. } => SurfaceExpression::Field {
            expr: Some(core_expr_to_surface_node(target)),
            field: key.clone(),
            resolution: crate::ast::Resolution::new(),
        },
        CoreExpr::Sequential { exprs, .. } => SurfaceExpression::Sequential(
            exprs.iter().map(|e| core_expr_to_surface_node(e)).collect(),
        ),
        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
            ..
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
            clauses, desugared, ..
        } => {
            // core_expr_to_surface_expr round-trips CoreExpr::Fn back to SurfaceExpression::Fn.
            // Only single-clause fns are round-tripped; multi-clause fns (instance methods)
            // are not produced by this path (only called from profiling/debug serialization).
            assert!(
                clauses.len() == 1,
                "core_expr_to_surface_expr: multi-clause Fn round-trip not supported"
            );
            let clause = clauses.first().expect("Fn must have at least one clause");
            SurfaceExpression::Fn {
                return_ann: None,
                params: clause
                    .params
                    .iter()
                    .map(|p| {
                        crate::ast::Spanned::new(
                            crate::ast::SurfaceParam {
                                name: p.node.name.clone(),
                                annotation: p.node.annotation.clone(),
                                variadic: p.node.variadic,
                                resolved_annotation_type: crate::ast::TypeAnnotation::new(),
                                resolved_dispatch_slot: crate::ast::ResolvedDispatchSlot::new(),
                            },
                            p.span.clone(),
                        )
                    })
                    .collect(),
                body: core_expr_to_surface_node(&clause.body),
                desugared: *desugared,
                resolved_return_annotation: crate::ast::TypeAnnotation::new(),
                scope: std::sync::OnceLock::new(),
            }
        }
        CoreExpr::TypeAssert { check, expr, .. } => {
            // Both TypeAssertCheck variants round-trip to SurfaceExpression::TypeAssert.
            // Source: preserve the original annotation for quote/unquote evaluation.
            // Resolved: no annotation to preserve — use a synthetic placeholder annotation.
            let annotation = match check {
                crate::ast::TypeAssertCheck::Source { annotation } => annotation.clone(),
                crate::ast::TypeAssertCheck::Resolved(_) => crate::ast::Spanned::new(
                    crate::ast::Annotation::Simple("_".to_string()),
                    rust_span!(),
                ),
            };
            SurfaceExpression::TypeAssert {
                annotation,
                expr: core_expr_to_surface_node(expr),
                resolved_type: crate::ast::TypeAnnotation::new(),
            }
        }
        CoreExpr::Rest { name, .. } => SurfaceExpression::Placeholder(name.clone(), None),
        CoreExpr::Match {
            scrutinee, arms, ..
        } => SurfaceExpression::Match {
            scrutinee: core_expr_to_surface_node(scrutinee),
            arms: arms
                .iter()
                .map(|arm| {
                    // Round-trip CoreClause → SurfaceMatchArm (T-2128).
                    let syn_span = rust_span!();
                    let let_bindings = if arm.params.is_empty() {
                        Arc::new(SurfaceNode::new(
                            SurfaceExpression::PatternDecl { bindings: vec![] },
                            syn_span.clone(),
                        ))
                    } else {
                        // Reconstruct [let name1 name2 ...] from params.
                        let binding_nodes: Vec<Arc<SurfaceNode>> = arm
                            .params
                            .iter()
                            .map(|p| {
                                Arc::new(SurfaceNode::new(
                                    SurfaceExpression::VarRef {
                                        name: p.node.name.clone(),
                                        escaped: false,
                                        resolution: crate::ast::Resolution::new(),
                                        annotation: None,
                                        do_infer_placeholder: false,
                                    },
                                    p.span.clone(),
                                ))
                            })
                            .collect();
                        Arc::new(SurfaceNode::new(
                            SurfaceExpression::LetDecl {
                                bindings: binding_nodes,
                            },
                            syn_span.clone(),
                        ))
                    };
                    // Pattern: reconstruct from lowered_pattern if present, else a wildcard.
                    let pattern = match &arm.lowered_pattern {
                        Some(lp) => core_expr_to_surface_node(lp),
                        None => Arc::new(SurfaceNode::new(
                            SurfaceExpression::Placeholder(None, None),
                            syn_span.clone(),
                        )),
                    };
                    SurfaceMatchArm {
                        pattern,
                        let_bindings,
                        guard: arm.guard.as_ref().map(|g| core_expr_to_surface_node(g)),
                        body: vec![core_expr_to_surface_node(&arm.body)],
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                        scope: std::sync::OnceLock::new(),
                    }
                })
                .collect(),
        },
        CoreExpr::Quote { expr: inner, .. } => {
            SurfaceExpression::Quote(core_expr_to_surface_node(inner))
        }
        CoreExpr::Unquote { expr: inner, .. } => {
            SurfaceExpression::Unquote(core_expr_to_surface_node(inner))
        }
        CoreExpr::UnquoteSplice { expr: inner, .. } => {
            SurfaceExpression::UnquoteSplice(core_expr_to_surface_node(inner))
        }
        CoreExpr::PatternDecl { bindings, .. } => SurfaceExpression::PatternDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::LetDecl { bindings, .. } => SurfaceExpression::LetDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::Dict { entries, .. } => SurfaceExpression::Dict {
            entries: entries
                .iter()
                .map(|e| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceEntry {
                            key: e.node.key.as_ref().map(|k| core_expr_to_surface_node(k)),
                            value: core_expr_to_surface_node(&e.node.value),
                            assigned_slot: std::sync::OnceLock::new(),
                        },
                        e.span.clone(),
                    )
                })
                .collect(),
            scope: std::sync::OnceLock::new(),
        },
        CoreExpr::Placeholder { .. } => SurfaceExpression::Placeholder(None, None),
        // Variant: emitted by lower.rs for type declarations; not user-writable in quotes.
        // Represent as a VarRef to the tag so quote round-trips see a name.
        CoreExpr::Variant {
            type_decl_id,
            ctor_index,
            ..
        } => {
            let empty = std::collections::HashMap::new();
            SurfaceExpression::VarRef {
                name: crate::type_tags::ctor_display_name(*type_decl_id, *ctor_index, &empty),
                escaped: false,
                resolution: crate::ast::Resolution::new(),

                annotation: None,
                do_infer_placeholder: false,
            }
        }
        CoreExpr::UnitVariant {
            type_decl_id,
            ctor_index,
            ..
        } => {
            let empty = std::collections::HashMap::new();
            SurfaceExpression::VarRef {
                name: crate::type_tags::ctor_display_name(*type_decl_id, *ctor_index, &empty),
                escaped: false,
                resolution: crate::ast::Resolution::new(),

                annotation: None,
                do_infer_placeholder: false,
            }
        }
        // ReprDecl: transparent in quote context — convert as if it were just the inner dict.
        // The repr: metadata is evaluator-only; it has no surface representation in quotes.
        CoreExpr::ReprDecl { inner, .. } => core_expr_to_surface_expr(&inner.node),
        // TypeDecl: transparent in quote context — convert as if it were just the inner dict.
        // The type identity mechanism is evaluator-only (ctx.type_identity_registry).
        CoreExpr::TypeDecl { inner, .. } => core_expr_to_surface_expr(&inner.node),
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
    diagnostics: &mut Vec<Diagnostic>,
    type_display_names: &std::collections::HashMap<u64, String>,
) -> Spanned<CoreExpr> {
    let span = arc.span.clone();
    let core_expr = match &arc.expr {
        // Declaration name forms: lower as string literal (name extraction path)
        // Annotated VarRef (name@Type) is also lowered to Str — the annotation is stripped.
        SurfaceExpression::VarRef { name, .. } => CoreExpr::Str {
            value: name.clone(),
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
        },
        SurfaceExpression::Placeholder(Some(name), _) => CoreExpr::Str {
            value: name.clone(),
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
        },
        // Wildcard / unnamed rest: use empty string (skipped by LetDecl eval arm)
        SurfaceExpression::Placeholder(None, _) => CoreExpr::Str {
            value: String::new(),
            inferred_type: make_typevalue_nominal(crate::value::TYPE_ID_STRING),
        },
        // All other forms: lower normally (will produce Error if unresolvable).
        _ => lower_expr(arc, &arc.expr, diagnostics, type_display_names),
    };
    Spanned::new(core_expr, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        Provenance, Resolution, Spanned, SurfaceDeclaration, SurfaceExpression, SurfaceItem,
        SurfaceNode, TypeAnnotation,
    };
    use std::sync::Arc;

    fn make_node(expr: SurfaceExpression, span: crate::ast::Span) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span,
            type_guard: TypeAnnotation::new(),
            provenance: Provenance::new(),
            inferred_type_decl_id: crate::ast::InferredType::new(),
            type_instantiation: crate::ast::TypeInstantiation::new(),
        })
    }

    #[test]
    fn test_lower_int_literal() {
        let span = rust_span!();
        let node = make_node(SurfaceExpression::Int(42), span.clone());

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (result, diagnostics) = lower_result.into_parts();
        let value = result.expect("lowering always succeeds");

        assert_eq!(value.span, span);
        assert!(matches!(value.node, CoreExpr::Int { value: 42, .. }));
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_lower_varref_with_resolution() {
        let span = rust_span!();
        // Build a VarRef node with pre-set inline resolution (Dispatch(0, 3)).
        let resolution = Resolution::new();
        resolution.set(Some(VarAddr::Dispatch(0, 3)));
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "x".into(),
                escaped: false,
                resolution,

                annotation: None,
                do_infer_placeholder: false,
            },
            span,
        );

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (result, diagnostics) = lower_result.into_parts();
        let value = result.expect("lowering always succeeds");

        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics
        );
        match value.node {
            CoreExpr::Var { name, addr, .. } => {
                assert_eq!(name, "x");
                // resolution was Dispatch(0, 3) → addr is the same
                assert_eq!(addr, VarAddr::Dispatch(0, 3));
            }
            _ => panic!("expected CoreExpr::Var, got {:?}", value.node),
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

                annotation: None,
                do_infer_placeholder: false,
            },
            span.clone(),
        );

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (result, diagnostics) = lower_result.into_parts();
        let value = result.expect("lowering always succeeds");

        // Unresolvable VarRef produces CoreExpr::Placeholder and a diagnostic.
        assert!(
            matches!(value.node, CoreExpr::Placeholder { .. }),
            "expected CoreExpr::Placeholder for unresolvable VarRef, got {:?}",
            value.node
        );
        // The error must be reported as a diagnostic.
        assert_eq!(
            diagnostics.len(),
            1,
            "expected exactly one diagnostic for unresolvable VarRef"
        );
        assert!(
            diagnostics[0].level == crate::error::DiagnosticLevel::Err,
            "expected Error diagnostic, got {:?}",
            diagnostics[0].level
        );
        assert!(
            diagnostics[0].message.contains("unbound"),
            "diagnostic message should mention the variable name: {}",
            diagnostics[0].message
        );
    }

    // Standalone Decl(TypeAlias) lowers to Placeholder.
    //
    // After the expand stage was introduced (T-2219), TypeAlias declarations are expanded
    // into UnitVariant/MakeVariant/TypeDecl/ReprDecl nodes before reaching the lowerer.
    // Any remaining Decl in expression position produces Placeholder.
    #[test]
    fn test_lower_type_alias_standalone_returns_placeholder() {
        let span = rust_span!();
        let body = make_node(
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
                resolution: Resolution::new(),

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

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (result, _diagnostics) = lower_result.into_parts();
        let value = result.expect("lowering always succeeds");

        assert!(
            matches!(value.node, CoreExpr::Placeholder { .. }),
            "standalone Decl(TypeAlias) must lower to Placeholder, got {:?}",
            value.node
        );
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
            let lower_result = lower(node, &std::collections::HashMap::new());
            let (result, _diagnostics) = lower_result.into_parts();
            let value = result.expect("lowering always succeeds");
            entries.push((format!("{expr_idx}"), Arc::new(value)));
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

    #[test]
    fn test_builtin_raise_name_registered_in_core_builtins() {
        // Verify that a builtin with name BUILTIN_RAISE_NAME is actually registered in
        // core_builtins(). The builtin! macro requires a literal, so the registration in
        // builtins_core.rs cannot directly reference the constant — this test catches drift.
        let builtins = crate::builtins_core::core_builtins();
        assert!(
            builtins.iter().any(|b| b.name == BUILTIN_RAISE_NAME),
            "BUILTIN_RAISE_NAME '{}' is not registered in core_builtins(); \
             update the registration literal in builtins_core.rs",
            BUILTIN_RAISE_NAME
        );
    }

    // ── T-2142: trace annotation — lowering phase emits trace-lower diagnostic ─────────────

    /// Build a minimal `SurfaceExpression::Fn` whose return annotation is
    /// `@[trace: N]` (a PropertyDict with a single `trace` key).
    ///
    fn make_trace_fn_node(trace_value: i64, span: crate::ast::Span) -> Arc<SurfaceNode> {
        use crate::ast::{Annotation, SurfaceEntry, SurfaceParam, TypeAnnotation};

        // Build the `trace: N` entry for the PropertyDict annotation.
        // get_property() matches SurfaceExpression::StringLiteral { content, .. } as key.
        let key_node = Arc::new(SurfaceNode::new(
            SurfaceExpression::StringLiteral {
                prefix: String::new(),
                delimiter: "\"".to_string(),
                content: crate::ast::ANNOTATION_KEY_TRACE.to_string(),
            },
            span.clone(),
        ));
        let value_node = Arc::new(SurfaceNode::new(
            SurfaceExpression::Int(trace_value),
            span.clone(),
        ));
        let trace_entry = Spanned::new(
            SurfaceEntry {
                key: Some(key_node),
                value: value_node,
                assigned_slot: std::sync::OnceLock::new(),
            },
            span.clone(),
        );
        let return_ann = Some(Spanned::new(
            Annotation::PropertyDict(vec![trace_entry]),
            span.clone(),
        ));

        // Body is a trivial integer literal — no complex lowering needed.
        let body = Arc::new(SurfaceNode::new(SurfaceExpression::Int(42), span.clone()));

        // Empty param list — no typed params means no param TypeAssert checks.
        let params: Vec<Spanned<SurfaceParam>> = Vec::new();

        Arc::new(SurfaceNode::new(
            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared: false,
                resolved_return_annotation: TypeAnnotation::new(),
                scope: std::sync::OnceLock::new(),
            },
            span,
        ))
    }

    /// T-2142: A function with @[trace: 1] annotation must emit a "trace" Info diagnostic
    /// from the lowering pass.
    ///
    /// Proof this is a Rust unit test (category 1): `lower()` returns `(Spanned<CoreExpr>,
    /// Vec<Diagnostic>)`. The `Vec<Diagnostic>` is an internal Rust value; it is not
    /// surface-observable by running tinct code. The corpus runner observes JSON output from eval,
    /// not lowering-phase diagnostics.
    ///
    /// Mutation target: if the `is_some_and(|ann| ann.node.get_property(...).is_some())` guard
    /// were removed, trace-lower would fire on all functions, breaking
    /// `test_no_trace_lower_without_annotation` below. If it were inverted (`.is_none()`),
    /// this test would fail.
    #[test]
    fn test_trace_annotation_lower_emits_diagnostic() {
        let span = rust_span!();
        let node = make_trace_fn_node(1, span);

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (_result, diagnostics) = lower_result.into_parts();

        let trace_diags: Vec<_> = diagnostics.iter().filter(|d| d.kind == "trace").collect();
        assert!(
            !trace_diags.is_empty(),
            "Expected at least one trace-lower diagnostic for @[trace: 1] fn; got: {:?}",
            diagnostics
        );
        assert_eq!(
            trace_diags[0].level,
            crate::error::DiagnosticLevel::Info,
            "trace-lower diagnostic must be Info level; got: {:?}",
            trace_diags[0].level
        );
    }

    /// T-2142 / fix-review S-1023: The trace-lower diagnostic requires trace: value >= 1,
    /// consistent with the typecheck and runtime phases. `trace: 0` must NOT emit trace-lower.
    ///
    /// Previously this test asserted the opposite (key-presence check). The guard was changed
    /// to value-based (`>= 1`) so all three trace phases behave uniformly.
    #[test]
    fn test_trace_lower_zero_does_not_fire() {
        let span = rust_span!();
        // trace: 0 — value 0 must NOT trigger trace-lower (value-based guard, same as typecheck)
        let node = make_trace_fn_node(0, span);

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (_result, diagnostics) = lower_result.into_parts();

        let trace_diags: Vec<_> = diagnostics.iter().filter(|d| d.kind == "trace").collect();
        assert!(
            trace_diags.is_empty(),
            "Expected NO trace-lower diagnostic for @[trace: 0] — guard requires >= 1; got: {:?}",
            diagnostics
        );
    }

    /// T-2142: A function with NO trace annotation must NOT emit a trace-lower diagnostic.
    ///
    /// Clean-path invariant: without this test, a mutation that emits trace-lower unconditionally
    /// would pass `test_trace_annotation_lower_emits_diagnostic` but fail here.
    #[test]
    fn test_no_trace_lower_without_annotation() {
        use crate::ast::{SurfaceParam, TypeAnnotation};

        let span = rust_span!();

        // Plain function with NO return annotation — return_ann is None.
        let body = Arc::new(SurfaceNode::new(SurfaceExpression::Int(42), span.clone()));
        let params: Vec<Spanned<SurfaceParam>> = Vec::new();

        let node = Arc::new(SurfaceNode::new(
            SurfaceExpression::Fn {
                return_ann: None,
                params,
                body,
                desugared: false,
                resolved_return_annotation: TypeAnnotation::new(),
                scope: std::sync::OnceLock::new(),
            },
            span,
        ));

        let lower_result = lower(&node, &std::collections::HashMap::new());
        let (_result, diagnostics) = lower_result.into_parts();

        let trace_diags: Vec<_> = diagnostics.iter().filter(|d| d.kind == "trace").collect();
        assert!(
            trace_diags.is_empty(),
            "Expected zero trace-lower diagnostics for unannotated function; got: {:?}",
            trace_diags
        );
    }
}
