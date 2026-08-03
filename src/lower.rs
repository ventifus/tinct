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
//! - `Field` → `Call(builtin-dict-get, [Str/Int(key), target])` (unified key-based lookup)
//! - `Dict` with spread entries → nested `Call(builtin-dict-merge, [seg, rest])` (Axiom 4: core builtin, not prelude name)
//! - `SurfaceNode.type_guard` set → wraps the lowered CoreExpr in `CoreExpr::TypeAssert`
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::ast::{
    class_decl_name, CoreClause, CoreEntry, CoreExpr, CoreNamedArg, CoreParam, Span, Spanned,
    SurfaceEntry, SurfaceExpression, SurfaceNode, VarAddr,
};
use crate::error::Diagnostic;
use crate::eval_call::is_typevalue_unknown;
use crate::rust_span;
use crate::type_infer::{make_typevalue_fn_with_flags, make_typevalue_unknown};

/// Global counter for unique type declaration IDs.
/// Each `[type Name ...]` declaration gets a unique ID at lower time via `next_type_decl_id()`.
/// This ID is threaded through TypeDecl, UnitVariant, and Variant nodes so the evaluator can
/// correctly associate variants with their parent type's identity (in `type_identity_registry`)
/// even when multiple types share the same name in different scopes.
///
/// Starts at TYPE_ID_RESERVED_MAX + 1 (100) to avoid collisions with primitive type IDs (1..=99).
static TYPE_DECL_ID_COUNTER: AtomicU64 = AtomicU64::new(crate::value::TYPE_ID_RESERVED_MAX + 1);

/// Returns the next globally unique type declaration ID.
/// User-defined variants get IDs starting at 100; primitive Value variants use IDs 1..=99.
fn next_type_decl_id() -> u64 {
    TYPE_DECL_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// The tinct name of the dict-field accessor builtin.
///
/// Every dot-access (`expr.field`) desugars to `[FIELD_GETTER_NAME key target]`. The
/// resolver writes the VarAddr for this name into `Field.resolution` (via a seed frame
/// built from `root_group_resolver_map()`); the lowerer reads it out and emits a
/// `CoreExpr::Var` referencing the builtin at its correct runtime slot. Centralising the
/// name here prevents the string from drifting between the resolver, the lowerer, and
/// diagnostic messages.
pub(crate) const FIELD_GETTER_NAME: &str = "builtin-dict-get";

/// Name of the core builtin used to implement spread-dict desugaring.
///
/// Spread-dict (`[...rest  key: val]`) desugars to nested `builtin-dict-merge` calls.
/// Using a core builtin name ensures spread-dict works regardless of which prelude (if any)
/// is loaded — it is resolved from the root group (always present) rather than user scope.
pub(crate) const DICT_MERGE_NAME: &str = "builtin-dict-merge";
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

/// Resolve `name` in ancestor scopes using "second occurrence" semantics.
///
/// Algorithm: skip the first (innermost) frame containing `name`, then return the slot
/// from the next frame that contains `name`. The ambient scope frame (outermost) is a
/// valid parent — if no ancestor dict defines `name`, we can fall back to it.
///
/// Used for MethodDispatcher parent chaining: the current dict already has a binding for
/// `method_name` (the dispatcher being synthesized). This function skips that binding and
/// returns the next-outer one (the parent dispatcher to delegate to, or the ambient scope
/// fallback if no ancestor dict has it).
///
/// Returns `None` if `name` appears in at most one frame, or not at all.

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
/// - `resolve_name_in_frames` results (scope frame lookups for spread-dict `builtin-dict-merge`)
/// - Synthetic addresses for lowerer-generated nodes (constructor functions, builtin-make-annotated)
///
/// Resolver-produced `Resolution` cells now store `VarAddr` directly and do not use this function.
///
/// Mapping:
/// - level=0 → `VarAddr::Dispatch(0, slot)` (current letrec group)
/// - level>0 → `VarAddr::ClosureCapture(slot)` (outer scope)
pub(crate) fn debruijn_to_var_addr(level: u32, slot: u32) -> VarAddr {
    if level == 0 {
        VarAddr::Dispatch(0, slot)
    } else {
        VarAddr::ClosureCapture(slot)
    }
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
///
/// `scope_frames` — when `Some`, provides the accumulated resolver scope frames from the
/// init program's resolver run. Used to resolve scope-frame-dependent names (e.g.,
/// `builtin-dict-merge` for spread dicts) to correct De Bruijn coordinates. Pass `None`
/// when the EvalContext was not initialized via `with_scope_frames()` (test contexts,
/// bootstrap paths).
pub fn lower(
    arc: &Arc<SurfaceNode>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> (Spanned<CoreExpr>, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let spanned = lower_inner(arc, &mut diagnostics, scope_frames);
    (spanned, diagnostics)
}

/// Internal lowering entry point that threads the diagnostics accumulator and scope frames.
///
/// Used by recursive calls within lower.rs and by eval machinery that does not need the
/// diagnostic Vec. When a VarRef or parse error is encountered, a `Diagnostic` is
/// pushed and `CoreExpr::Placeholder` is emitted. Produces the same `Spanned<CoreExpr>`
/// as the public `lower()`.
///
/// `scope_frames` is threaded through all recursive calls so that scope-frame-dependent
/// name resolution (e.g., `builtin-dict-merge` for spread dicts) can resolve to correct
/// De Bruijn coordinates. Pass `None` when scope frames are not available.
pub(crate) fn lower_inner(
    arc: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<Diagnostic>,
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

    // Apply type guard if the type checker set one on this node (T-2013).
    // Use TypeAssertCheck::Resolved — the guard_type is already a resolved TypeValue.
    // No annotation sentinel needed; the check field carries the TypeValue directly.
    let core_expr = if let Some(guard_type) = arc.type_guard.get() {
        CoreExpr::TypeAssert {
            expr: Arc::new(crate::ast::Spanned::new(core_expr, span.clone())),
            check: crate::ast::TypeAssertCheck::Resolved(guard_type.clone()),
            pipeline_blame: None,
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
            assert!(
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
                    // The sentinel addr ClosureCapture(u32::MAX) is never assigned by
                    // the resolver to real bindings (capture indices are assigned 0..N-1).
                    // In pattern position, bind_or_pin_name detects this sentinel and
                    // returns Ok(false) (non-match). In body position, eval_core's Var
                    // arm looks up closure_env[u32::MAX as usize] → None →
                    // EvalError::undefined_variable, which is the correct deferred error
                    // for referencing an undefined name.
                    if name == "_" {
                        CoreExpr::Placeholder
                    } else {
                        CoreExpr::Var {
                            name: name.clone(),
                            addr: VarAddr::ClosureCapture(u32::MAX),
                            annotation: annotation.clone(),
                        }
                    }
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
                    diagnostics.push(Diagnostic::error("lower-error", message, arc.span.clone()));
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
            // The resolver writes a VarAddr for FIELD_GETTER_NAME into Field.resolution
            // (because the resolver is always seeded with root_group via root_group_resolver_map).
            // All dot-access desugars to [FIELD_GETTER_NAME key target] — one correct path.
            let getter_addr = match resolution.get() {
                Some(Some(addr)) => addr.clone(),
                _ => {
                    diagnostics.push(Diagnostic::error(
                        "lower-error",
                        format!(
                            "{FIELD_GETTER_NAME}: resolver did not populate Field.resolution for `.{}` — resolver must be seeded with root_group",
                            field
                        ),
                        arc.span.clone(),
                    ));
                    return CoreExpr::Placeholder;
                }
            };

            let key_core = match field {
                crate::ast::DotKey::Int(n) => CoreExpr::Int(*n),
                crate::ast::DotKey::Ident(s) => CoreExpr::Str(s.clone()),
            };

            let getter_var = Arc::new(crate::ast::Spanned::new(
                CoreExpr::Var {
                    name: FIELD_GETTER_NAME.to_string(),
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
                diagnostics.push(Diagnostic::error(
                    "lower-error",
                    format!("undefined variable: .{}", name),
                    arc.span.clone(),
                ));
                CoreExpr::Placeholder
            }
            None => {
                // Resolver ran but did not set coordinates for this leading-dot reference.
                // This happens when the resolver skipped this node's enclosing scope
                // (e.g. inside a TypeAlias body). The name cannot be resolved — emit
                // a diagnostic rather than silently producing a MAX/MAX sentinel.
                diagnostics.push(Diagnostic::error(
                    "lower-error",
                    format!("undefined variable: .{}", name),
                    arc.span.clone(),
                ));
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
            diagnostics.push(Diagnostic::error(
                "lower-error",
                "leading-dot integer access is not supported".to_string(),
                arc.span.clone(),
            ));
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
            // Check for spread entries (...expr) — desugar to builtin-dict-merge calls.
            // [a: 1  b: 2  ...rest  c: 3] → builtin-dict-merge(builtin-dict-merge([a: 1  b: 2], rest), [c: 3])
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

                // Build nested builtin-dict-merge calls left-associatively.
                // acc starts as the first segment dict, then folds over (rest, next_seg) pairs.
                let span = arc.span.clone();

                // Resolve `builtin-dict-merge` through scope_frames to get correct de Bruijn
                // coordinates. This is a core builtin (always in the root frame) so it is
                // independent of which prelude (if any) is loaded — Axiom 4 compliant.
                //
                // CALLER REQUIREMENT: scope_frames must be seeded with the root_group frame
                // (via `resolve_surface_document_with_seed_frames` or equivalent). Without it,
                // `builtin-dict-merge` is not resolvable and a Placeholder + error is emitted.
                // All production call sites (formatter.rs, builtin_lower in builtins_meta.rs)
                // must seed scope_frames before calling lower/lower_inner on spread-dict expressions.
                let (merge_level, merge_slot) = match scope_frames
                    .and_then(|frames| resolve_name_in_frames(frames, DICT_MERGE_NAME))
                {
                    Some(coords) => coords,
                    None => {
                        diagnostics.push(Diagnostic::error(
                            "lower-error",
                            format!(
                                "spread-dict desugaring: '{DICT_MERGE_NAME}' not found in scope frames — resolver must be seeded with root_group"
                            ),
                            span.clone(),
                        ));
                        return CoreExpr::Placeholder;
                    }
                };

                let mut acc = lower_seg!(&segments[0]);
                for (i, &ri) in rest_indices.iter().enumerate() {
                    // lower_inner returns Spanned<CoreExpr> — wrap in Arc directly.
                    let rest_spanned =
                        lower_inner(&entries[ri].node.value, diagnostics, scope_frames);
                    // builtin-dict-merge(acc, rest)
                    acc = CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::Var {
                                name: DICT_MERGE_NAME.to_string(),
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
                    // builtin-dict-merge(acc, next_segment) if non-empty
                    if i + 1 < segments.len() && !segments[i + 1].is_empty() {
                        let seg = lower_seg!(&segments[i + 1]);
                        acc = CoreExpr::Call {
                            func: Arc::new(Spanned::new(
                                CoreExpr::Var {
                                    name: DICT_MERGE_NAME.to_string(),
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

            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            for se in entries {
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::InstanceDecl {
                            class_name,
                            arms,
                            resolved_class_decl_id,
                            ..
                        } => {
                            {
                                // Both named and anonymous instances:
                                // 1. If named, emit the outer key binding first.
                                // 2. Emit mangled binding slots (dispatch) — plain method slots are
                                //    no longer emitted; EffectPerform scans the group instead.
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
                                        // The plain method slot (`<` itself) is no longer emitted.
                                        // VarAddr::EffectPerform scans the accumulated group for
                                        // Value::Function { instance_of: Some((class_id, method)) }
                                        // at call time — no synthesized dispatcher needed.

                                        // Mangled binding slot for dispatch.
                                        // Lower the method body and stamp instance_of so the
                                        // evaluator can locate this function via EffectPerform scan.
                                        let binding_name = crate::type_def::instance_binding_name(
                                            &class_decl_name(class_name),
                                            &method_name,
                                            &type_args,
                                        );
                                        let key = Some(Arc::new(Spanned::new(
                                            CoreExpr::Str(binding_name),
                                            se.span.clone(),
                                        )));
                                        // class_decl_id is written by the type checker via
                                        // resolved_class_decl_id. If absent, the type checker did
                                        // not run — emit an error and skip this method entry.
                                        let class_decl_id: u64 = match resolved_class_decl_id.get()
                                        {
                                            Some(id) => id,
                                            None => {
                                                diagnostics.push(Diagnostic::error(
                                                    "lower-error",
                                                    format!(
                                                        "instance declaration: class_decl_id not set \
                                                         for method '{}' — type checker must run \
                                                         before lowering instances",
                                                        method_name
                                                    ),
                                                    se.span.clone(),
                                                ));
                                                continue;
                                            }
                                        };
                                        let mut lowered_value =
                                            lower_inner(&me.node.value, diagnostics, scope_frames);
                                        // If the method body lowered to a CoreExpr::Fn, stamp
                                        // instance_of = Some((class_decl_id, method_name)) on it.
                                        // This is the discriminator VarAddr::EffectPerform uses
                                        // when scanning the accumulated group for matching impls.
                                        lowered_value.node = match lowered_value.node {
                                            CoreExpr::Fn {
                                                clauses,
                                                captures,
                                                instance_of: _,
                                                resolved_fn_type,
                                                desugared,
                                                return_ann,
                                            } => CoreExpr::Fn {
                                                clauses,
                                                captures,
                                                instance_of: Some((
                                                    class_decl_id,
                                                    method_name.clone(),
                                                )),
                                                resolved_fn_type,
                                                desugared,
                                                return_ann,
                                            },
                                            // Non-Fn body (e.g., a VarRef to an existing function):
                                            // wrap in a zero-param Fn so instance_of is stamped.
                                            // VarAddr::EffectPerform scans for Value::Function with
                                            // instance_of=Some((class_id, method)) — the stamp must
                                            // always be present regardless of the body expression type.
                                            other => {
                                                let body_expr = Arc::new(Spanned::new(
                                                    other,
                                                    lowered_value.span.clone(),
                                                ));
                                                let wrapper_clause = CoreClause {
                                                    params: vec![],
                                                    lowered_pattern: None,
                                                    guard: None,
                                                    body: body_expr,
                                                    guard_matchable_binding:
                                                        crate::ast::MatchableBinding::new(),
                                                    captures: Arc::new(vec![]),
                                                };
                                                CoreExpr::Fn {
                                                    clauses: vec![wrapper_clause],
                                                    captures: Arc::new(vec![]),
                                                    instance_of: Some((
                                                        class_decl_id,
                                                        method_name.clone(),
                                                    )),
                                                    resolved_fn_type: None,
                                                    desugared: false,
                                                    return_ann: None,
                                                }
                                            }
                                        };
                                        let value = Arc::new(lowered_value);
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
                            SurfaceExpression::StringLiteral { content, .. } => {
                                Some(content.as_str())
                            }
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
                                        "lower-error",
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
        } => CoreExpr::Call {
            func: Arc::new(lower_inner(func, diagnostics, scope_frames)),
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
                            value: Arc::new(lower_inner(&na.node.value, diagnostics, scope_frames)),
                        },
                        na.span.clone(),
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
            resolved_captures,
            resolved_return_annotation,
        } => {
            // Lower the body first to get its span for wrapping.
            let lowered_body_spanned = lower_inner(body, diagnostics, scope_frames);
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
                            let param_ref = CoreExpr::Var {
                                name: p.node.name.clone(),
                                addr: VarAddr::Parameter(i as u32),
                                annotation: None,
                            };
                            let assert = CoreExpr::TypeAssert {
                                expr: Arc::new(crate::ast::Spanned::new(param_ref, p.span.clone())),
                                check: crate::ast::TypeAssertCheck::Resolved(tv.clone()),
                                pipeline_blame: None,
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
                    CoreExpr::Sequential(param_assert_exprs),
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
                    },
                    body_span,
                )
            } else {
                body_with_param_checks
            };

            // Build the resolved TypeValue.Fn so the runtime can store it on the function
            // value and `ground_typevalue_of` can use it for full structural type checking.
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
                    .map(|tv| crate::eval::format_type_for_assert(tv))
                    .unwrap_or_else(|| "<no type>".to_string());
                diagnostics.push(Diagnostic::info("trace-lower", sig, arc.span.clone()));
            }

            let fn_captures = resolved_captures
                .get()
                .expect("resolved_captures not set")
                .clone();
            let clause = crate::ast::CoreClause {
                params: params_built,
                lowered_pattern: None,
                guard: None,
                body: Arc::new(final_body),
                guard_matchable_binding: crate::ast::MatchableBinding::new(),
                captures: fn_captures.clone(),
            };
            CoreExpr::Fn {
                clauses: vec![clause],
                captures: fn_captures,
                instance_of: None,
                desugared: *desugared,
                resolved_fn_type,
                // Carry return_ann into CoreExpr::Fn so the evaluator can populate
                // FnAnnotation.extra at function-definition time.
                // This enables annotation-driven runtime behavior: trace:, as-type:, etc.
                return_ann: return_ann.clone(),
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
                expr: Arc::new(lower_inner(inner, diagnostics, scope_frames)),
                check,
                pipeline_blame: None,
            }
        }

        SurfaceExpression::Placeholder(Some(name), _) => CoreExpr::Rest(Some(name.clone())),
        SurfaceExpression::Placeholder(None, _) => CoreExpr::Placeholder,

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower_inner(scrutinee, diagnostics, scope_frames)),
            arms: arms
                .iter()
                .map(|arm| {
                    // Build params from let_bindings for case arms (T-2128).
                    // For keyed arms (let_bindings == None), params is empty.
                    let params: Vec<crate::ast::Spanned<CoreParam>> =
                        if let Some(let_bindings) = &arm.let_bindings {
                            // Extract binding names from [let name1 name2 ...], mirroring
                            // resolve.rs::extract_case_arm_binding_names. The resolver assigns
                            // Parameter(i) to each name via enter_param_scope; lowering now
                            // materializes that as CoreParam entries (slot = i, variadic = false).
                            // `_` is excluded — it is a wildcard, not a binding.
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
                        } else {
                            vec![]
                        };
                    // Lower the arm pattern for ALL match arms (keyed and case).
                    // - Case arms (let_bindings.is_some()): lowered_pattern is the structural
                    //   filter expression evaluated against the arm's binding scope.
                    // - Keyed arms (let_bindings.is_none()): lowered_pattern is the key
                    //   expression (e.g. CoreExpr::Int, CoreExpr::Str, CoreExpr::Placeholder)
                    //   used by MatchDispatch for equality matching via eval_structural_pattern_inner.
                    let lowered_pattern = Some(Arc::new(lower_inner(
                        &arm.pattern,
                        diagnostics,
                        scope_frames,
                    )));
                    let captures: Arc<Vec<(String, VarAddr)>> = if arm.let_bindings.is_some() {
                        arm.case_captures
                            .get()
                            .expect("resolver must set case_captures for every case arm")
                            .clone()
                    } else {
                        Arc::new(vec![])
                    };
                    CoreClause {
                        params,
                        lowered_pattern,
                        guard: arm
                            .guard
                            .as_ref()
                            .map(|g| Arc::new(lower_inner(g, diagnostics, scope_frames))),
                        // body is always a single node (parser wraps multi-body in Sequential).
                        body: Arc::new(lower_inner(arm.body_expr(), diagnostics, scope_frames)),
                        captures,
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                    }
                })
                .collect(),
        },

        SurfaceExpression::Quote(inner) => {
            // Quote captures AST data — VarRefs inside are symbols, not runtime bindings.
            // The resolver intentionally skips Quote bodies, so VarRefs inside will have
            // OnceLock=None. We must not emit "undefined variable" diagnostics for them.
            // scope_frames is passed as None inside Quote: quoted expressions are symbol
            // references, not runtime calls — coordinates are irrelevant.
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

        SurfaceExpression::Decl(decl) => match decl.as_ref() {
            crate::ast::SurfaceDeclaration::InstanceDecl {
                class_name,
                arms,
                resolved_class_decl_id,
                ..
            } => {
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
                        let class_decl_id: u64 = match resolved_class_decl_id.get() {
                            Some(id) => id,
                            None => {
                                diagnostics.push(Diagnostic::error(
                                    "lower-error",
                                    format!(
                                        "instance declaration: class_decl_id not set \
                                         for method '{}' — type checker must run \
                                         before lowering instances",
                                        method_name
                                    ),
                                    syn_span.clone(),
                                ));
                                continue;
                            }
                        };
                        let mut lowered_value =
                            lower_inner(&me.node.value, diagnostics, scope_frames);
                        lowered_value.node = match lowered_value.node {
                            CoreExpr::Fn {
                                clauses,
                                captures,
                                instance_of: _,
                                resolved_fn_type,
                                desugared,
                                return_ann,
                            } => CoreExpr::Fn {
                                clauses,
                                captures,
                                instance_of: Some((class_decl_id, method_name.clone())),
                                resolved_fn_type,
                                desugared,
                                return_ann,
                            },
                            other => other,
                        };
                        let value = Arc::new(lowered_value);
                        core_entries.push(Spanned::new(CoreEntry { key, value }, syn_span.clone()));
                    }
                }

                if !core_entries.is_empty() {
                    return CoreExpr::Dict(core_entries);
                }
                CoreExpr::Placeholder
            }
            crate::ast::SurfaceDeclaration::TypeAlias { .. } => {
                // Type declarations in standalone expression position produce no runtime value.
                // The dict-entry case (lower.rs Dict arm, line ~309) calls
                // lower_type_alias_to_constructor_dict to produce constructor entries under the
                // declared name. Here (direct Decl, no enclosing dict entry), the declaration is
                // not bound to any name so there are no constructor entries to emit — return {}.
                CoreExpr::Dict(vec![])
            }
            _ => CoreExpr::Placeholder,
        },

        SurfaceExpression::Error(span) => {
            diagnostics.push(Diagnostic::error(
                "lower-error",
                "parse error".to_string(),
                span.clone(),
            ));
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
                            },
                            p.span.clone(),
                        )
                    })
                    .collect(),
                body: core_expr_to_surface_node(&clause.body),
                desugared: *desugared,
                resolved_captures: crate::ast::CapturesCell::new(),
                resolved_return_annotation: crate::ast::TypeAnnotation::new(),
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
        CoreExpr::Rest(name) => SurfaceExpression::Placeholder(name.clone(), None),
        CoreExpr::Match { scrutinee, arms } => SurfaceExpression::Match {
            scrutinee: core_expr_to_surface_node(scrutinee),
            arms: arms
                .iter()
                .map(|arm| {
                    // Round-trip CoreClause → SurfaceMatchArm (T-2128).
                    // Case arms have params (non-empty) → reconstruct let_bindings node.
                    // Keyed arms have no params → let_bindings = None, no scope.
                    let syn_span = rust_span!();
                    let let_bindings = if arm.params.is_empty() {
                        None
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
                        Some(Arc::new(SurfaceNode::new(
                            SurfaceExpression::LetDecl {
                                bindings: binding_nodes,
                            },
                            syn_span.clone(),
                        )))
                    };
                    // Pattern: reconstruct from lowered_pattern if present, else a wildcard.
                    let pattern = match &arm.lowered_pattern {
                        Some(lp) => core_expr_to_surface_node(lp),
                        None => Arc::new(SurfaceNode::new(
                            SurfaceExpression::Placeholder(None, None),
                            syn_span.clone(),
                        )),
                    };
                    // Reconstruct case_captures cell — set to the clause's captures.
                    let case_captures = crate::ast::CapturesCell::new();
                    case_captures.set(Arc::clone(&arm.captures));
                    SurfaceMatchArm {
                        pattern,
                        let_bindings,
                        guard: arm.guard.as_ref().map(|g| core_expr_to_surface_node(g)),
                        body: vec![core_expr_to_surface_node(&arm.body)],
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                        case_captures,
                    }
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
        CoreExpr::Placeholder => SurfaceExpression::Placeholder(None, None),
        // Variant: emitted by lower.rs for type declarations; not user-writable in quotes.
        // Represent as a VarRef to the tag so quote round-trips see a name.
        CoreExpr::Variant { tag, .. } => SurfaceExpression::VarRef {
            name: tag.clone(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),

            annotation: None,
            do_infer_placeholder: false,
        },
        CoreExpr::UnitVariant { tycon, ctor, .. } => SurfaceExpression::VarRef {
            name: if tycon.is_empty() {
                ctor.clone()
            } else {
                format!("{}.{}", tycon, ctor)
            },
            escaped: false,
            resolution: crate::ast::Resolution::new(),

            annotation: None,
            do_infer_placeholder: false,
        },
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
        // No scope_frames needed here: LetDecl binding names are not call sites.
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
            match annotation_dispatch_tag(inner) {
                Some(inner_tag) => Some(format!("{}@{}", base_tag, inner_tag)),
                None => Some(base_tag),
            }
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
        .filter(|binding| {
            // Skip `bind: [a b c]` entries — these declare TypeVars for the instance pattern,
            // not class type parameter positions.
            //
            // `bind: [a]` inside [let ...] parses as a VarRef named "bind" with a PropertyDict
            // annotation (via the LetDecl `:` mechanism). ANY VarRef named "bind" with a
            // PropertyDict annotation is the TypeVar declaration — skip it.
            if let SurfaceExpression::VarRef {
                name, annotation, ..
            } = &binding.expr
            {
                if name.as_str() == "bind" {
                    if let Some(ann) = annotation {
                        if matches!(ann.node, crate::ast::Annotation::PropertyDict(_)) {
                            return false; // Filter out bind: TypeVar declaration
                        }
                    }
                }
            }
            true
        })
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
/// Build a `CoreExpr::Dict` representing field-annotations: `{field: {role: "One"|"MapValues"}, ...}`.
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
            // Inner dict: {role: "One"} or {role: "MapValues"}
            let role_key = Some(Arc::new(Spanned::new(
                CoreExpr::Str(crate::ast::ANNOTATION_KEY_CHILD_ROLE.to_string()),
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
            // Outer entry: field_name: {role: "One"|"MapValues"}
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
///
/// When the TypeAlias body contains a `repr: "Value::X"` key, the result is wrapped in
/// `CoreExpr::ReprDecl` so that the evaluator can register the TypeValue in
/// `ctx.repr_registry` when the declaration is first forced.
///
/// An optional `is: [fn [let x] ...]` key is passed through as `ReprDecl::is_pred` so the
/// evaluator can also register the predicate in `ctx.is_predicates`.
fn lower_type_alias_to_constructor_dict(
    type_name_opt: Option<String>,
    body: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<Diagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> CoreExpr {
    use crate::ast::CoreEntry;

    // Extract `repr:` and `is:` metadata from the body dict before extracting constructors.
    // These are lowercase named keys that are type declaration metadata, not constructors.
    let (repr_opt, is_pred_opt) =
        extract_repr_and_is_from_body(&body.expr, diagnostics, scope_frames);

    // Extract constructors from the body using the desugar.rs helpers.
    // Constructors are extracted inline from the body expression.
    let ctors = extract_constructors_from_body(&body.expr);

    // Generate a unique type_decl_id for this type declaration.
    // This ID will be threaded through TypeDecl, UnitVariant, and Variant nodes so that
    // variants can look up their parent type's identity in type_identity_registry by ID
    // instead of by name. This prevents same-name types in nested scopes from overwriting
    // each other's registry entries (B-714).
    let type_decl_id = next_type_decl_id();

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
            // Unit constructor: CoreExpr::UnitVariant { tycon, ctor, type_decl_id }
            // If the constructor carries a @[...] annotation (T-1121), wrap with make-annotated.
            let variant_call = Arc::new(Spanned::new(
                CoreExpr::UnitVariant {
                    tycon: tycon.clone(),
                    ctor: ctor_name.clone(),
                    type_decl_id,
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
            // The payload dict is the function body — params are at VarAddr::Parameter(i).
            // (Previously used debruijn_to_var_addr(1, idx) which produced ClosureCapture(idx),
            // but the constructor function has no captures — its closure_env is empty.
            // ClosureCapture(0) would resolve to None at runtime, causing UndefinedVariable.)
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
                            addr: VarAddr::Parameter(idx as u32),
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

            // Build the variant body: Variant { tag, payload: Some(payload_dict), type_decl_id }
            let variant_body = Arc::new(Spanned::new(
                CoreExpr::Variant {
                    tag: qualified_tag.clone(),
                    payload: Some(payload_dict),
                    type_decl_id,
                },
                syn_span.clone(),
            ));

            let fn_captures = Arc::new(vec![]);
            let ctor_clause = CoreClause {
                params: fn_params,
                lowered_pattern: None,
                guard: None,
                body: variant_body,
                guard_matchable_binding: crate::ast::MatchableBinding::new(),
                captures: fn_captures.clone(),
            };
            let fn_expr = Arc::new(Spanned::new(
                CoreExpr::Fn {
                    clauses: vec![ctor_clause],
                    captures: fn_captures,
                    instance_of: None,
                    desugared: false,
                    resolved_fn_type: None,
                    // Payload constructor: carry the qualified tag as return_ann so that
                    // pattern matching can identify the constructor tag from the function's
                    // FnAnnotation.return_ann without a special runtime "constructor" type.
                    // `Annotation::Simple(qualified_tag)` is the convention: pattern dispatch
                    // checks `ann.return_ann == Some(Simple(tag))` to extract the ctor tag.
                    return_ann: Some(crate::ast::Spanned::new(
                        crate::ast::Annotation::Simple(qualified_tag.clone()),
                        syn_span.clone(),
                    )),
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

    let inner_dict = CoreExpr::Dict(core_entries);

    // If the body had `repr:` metadata, wrap the constructor dict in ReprDecl so the
    // evaluator can register it in ctx.repr_registry when the declaration is first forced.
    let inner_expr = match repr_opt {
        Some(repr) => CoreExpr::ReprDecl {
            repr,
            is_pred: is_pred_opt.map(|expr| Arc::new(Spanned::new(expr, syn_span.clone()))),
            inner: Arc::new(Spanned::new(inner_dict, syn_span.clone())),
        },
        None => inner_dict,
    };

    // Wrap in TypeDecl when a type name is available.
    //
    // TypeDecl instructs the evaluator (eval_core.rs) to create a stable `Arc<Value>`
    // identity for this type and register it in `ctx.type_identity_registry[type_decl_id]`.
    // The identity is stamped on the returned constructor dict (`type_val`) and on every
    // `Value::Variant` produced by the type's constructors (`UnitVariant` and `Variant`
    // arms look it up from the registry by type_decl_id at construction time).
    //
    // This enables `Arc::ptr_eq(variant.type_val, dict.type_val)` in `match_pattern` to
    // correctly identify whether a scrutinee belongs to a given type — without any hidden
    // sentinel key in the dict.
    //
    // The use of type_decl_id (unique u64) instead of type_name (String) prevents same-name
    // types in nested scopes from overwriting each other's registry entries (B-714).
    match type_name_opt {
        Some(type_name) => CoreExpr::TypeDecl {
            type_name,
            type_decl_id,
            inner: Arc::new(Spanned::new(inner_expr, syn_span)),
        },
        None => inner_expr,
    }
}

/// Returns true if a `SurfaceExpression` annotation (on a VarRef body) contains
/// `is:` or `repr:` keys, indicating this is a structural alias declaration rather
/// than a unit constructor.
///
/// Used by `extract_constructors_from_body` to skip VarRef bodies that carry structural
/// alias metadata (so they are not mistakenly extracted as unit constructors).
fn has_structural_alias_metadata(ann: &crate::ast::Annotation) -> bool {
    // Check for PropertyDict with `is:` or `repr:` string-literal keys.
    let check_entries = |entries: &[Spanned<SurfaceEntry>]| {
        entries.iter().any(|e| {
            let Some(key_node) = &e.node.key else {
                return false;
            };
            matches!(
                &key_node.expr,
                SurfaceExpression::VarRef { name, .. } if name == "is" || name == "repr"
            )
        })
    };
    match ann {
        crate::ast::Annotation::PropertyDict(entries) => check_entries(entries),
        crate::ast::Annotation::Annotated(_, inner) => has_structural_alias_metadata(inner),
        _ => false,
    }
}

/// Extract `repr:` and `is:` metadata entries from a TypeAlias body.
///
/// Handles two body forms:
///
/// 1. **Dict body** — the body is a `[...]` dict containing lowercase-keyed metadata
///    entries alongside constructors:
///    - `repr: "Value::Int"` — identifies the Rust Value variant this type maps to
///    - `is: [fn [let x] ...]` — predicate function for testing membership
///    Both keys are lowercase and are NOT constructors; they are silently skipped by
///    `extract_constructors_from_body`.
///
/// 2. **VarRef@[PropertyDict] body** — the body is a VarRef with a PropertyDict annotation,
///    e.g., `Int@[is: predicate  repr: "Value::Int"]`. In this form, `repr:` and `is:` are
///    annotation keys on the VarRef rather than entries in a surrounding dict body.
///
/// Returns `(repr_opt, is_pred_opt)` — both are `None` when the respective key is absent.
fn extract_repr_and_is_from_body(
    body: &SurfaceExpression,
    diagnostics: &mut Vec<Diagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> (Option<String>, Option<CoreExpr>) {
    // VarRef@[PropertyDict] form: `Int@[is: predicate  repr: "Value::Int"]`
    // The annotation carries `repr:` and `is:` as PropertyDict entries.
    if let SurfaceExpression::VarRef {
        annotation: Some(ann),
        ..
    } = body
    {
        // Extract the PropertyDict from the annotation. Handles:
        // - Annotation::PropertyDict(entries) directly on the VarRef
        // - Annotation::Annotated(Simple("Int"), PropertyDict(entries)) — the common form
        let prop_dict_entries = match &ann.node {
            crate::ast::Annotation::PropertyDict(entries) => Some(entries),
            crate::ast::Annotation::Annotated(_, inner) => {
                if let crate::ast::Annotation::PropertyDict(entries) = inner.as_ref() {
                    Some(entries)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(entries) = prop_dict_entries {
            return extract_repr_and_is_from_entries(entries, diagnostics, scope_frames);
        }
    }

    let SurfaceExpression::Dict(entries) = body else {
        return (None, None);
    };

    extract_repr_and_is_from_entries(entries, diagnostics, scope_frames)
}

/// Shared implementation: scan a slice of `SurfaceEntry` for `repr:` and `is:` keys.
///
/// Used by `extract_repr_and_is_from_body` for both the Dict body form (entries are the
/// dict body) and the VarRef annotation form (entries are PropertyDict annotation entries).
fn extract_repr_and_is_from_entries(
    entries: &[Spanned<SurfaceEntry>],
    diagnostics: &mut Vec<Diagnostic>,
    scope_frames: Option<&[indexmap::IndexMap<String, u32>]>,
) -> (Option<String>, Option<CoreExpr>) {
    let mut repr_opt: Option<String> = None;
    let mut is_pred_opt: Option<CoreExpr> = None;

    for entry in entries {
        let Some(key_node) = &entry.node.key else {
            continue;
        };
        let key_name = match &key_node.expr {
            SurfaceExpression::VarRef { name, .. } => name.as_str(),
            _ => continue,
        };

        match key_name {
            "repr" => {
                // `repr:` value must be a string literal — the Value variant name.
                if let SurfaceExpression::StringLiteral { content, .. } = &entry.node.value.expr {
                    repr_opt = Some(content.clone());
                } else {
                    diagnostics.push(Diagnostic::error(
                        "lower-error",
                        "repr: value must be a string literal (e.g., \"Value::Int\")".to_string(),
                        entry.span.clone(),
                    ));
                }
            }
            "is" => {
                // `is:` value is an arbitrary expression (typically a function).
                // Lower it using lower_inner so VarRefs resolve correctly and type guards
                // are applied (consistent with how other dict value expressions are lowered).
                is_pred_opt = Some(lower_inner(&entry.node.value, diagnostics, scope_frames).node);
            }
            _ => {}
        }
    }

    (repr_opt, is_pred_opt)
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
    /// Maps field name → role string ("One" or "MapValues").
    /// Only populated for fields that carry `@Child` annotation.
    field_annotations: indexmap::IndexMap<String, String>,
}

/// Extract the child role from a `@Child` or `@Child@[role: "..."]` annotation.
///
/// Returns the value of the explicit `role:` key in the annotation when present, or `"One"` as
/// the default. The role is declared by the type author via annotation, not inferred from the
/// type expression. This keeps the lowerer agnostic to prelude-defined type constructor names.
///
/// Recognized roles:
/// - `"One"`       — the field holds a single child TypeNode (the default)
/// - `"MapValues"` — the field holds a map whose values are children (declared explicitly)
///
/// Usage in tinct source:
/// - `@Child` → role "One" (bare Simple annotation, no role key)
/// - `@Child@[role: "MapValues"]` → role "MapValues" (Annotated outer=Child, inner=PropertyDict)
fn child_role_from_annotation(ann: &crate::ast::Annotation) -> &'static str {
    // Check for an explicit role: key in the annotation. get_property delegates through
    // Annotated to the inner PropertyDict, so @Child@[role: "MapValues"] works correctly.
    if let Some(role_node) = ann.get_property(crate::ast::ANNOTATION_KEY_CHILD_ROLE) {
        if let SurfaceExpression::StringLiteral { content, .. } = &role_node.expr {
            match content.as_str() {
                "MapValues" => return "MapValues",
                "One" => return "One",
                _ => {} // unrecognized role value — fall through to default
            }
        }
    }
    "One"
}

/// Extract constructor information from a TypeAlias body.
///
/// Handles the common constructor forms:
/// 1. Bare VarRef uppercase → unit constructor (e.g., `Red`, `None`)
/// 2. Annotated uppercase → unit constructor with annotation
/// 3. Call with uppercase func + no named args → unit constructor (e.g., `[Ok a]`, `[Error String]`)
/// 6. Dict with named uppercase-key entries → payload/unit constructors:
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
            //
            // Exception: if the annotation contains `is:` or `repr:` keys, this is a
            // structural alias body (`Int@[is: pred  repr: "Value::Int"]`), not a
            // unit constructor. Skip it here — handled by extract_repr_and_is_from_body.
            SurfaceExpression::VarRef {
                name, annotation, ..
            } if is_ctor(name) => {
                // Check for structural alias metadata before treating as a constructor.
                if let Some(ann) = annotation {
                    if has_structural_alias_metadata(&ann.node) {
                        return;
                    }
                }
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
            // Call with uppercase func → unit constructor (form 3).
            // [Ok a] → Call { func: VarRef("Ok"), args: [VarRef("a")], named_args: [] } → unit
            // Old named-arg form [Circle r: Int] (form 4) and annotated positional form (form 5)
            // are both rejected at parse time by T-1539 and never reach this branch.
            SurfaceExpression::Call { func, .. } => {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if is_ctor(name) {
                        ctors.push(ConstructorInfo {
                            name: name.clone(),
                            is_unit: true,
                            annotation: None,
                            fields: vec![],
                            field_annotations: indexmap::IndexMap::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Top-level dispatch: distinguish body forms.
    match body {
        SurfaceExpression::Dict(entries) => {
            // New named-key constructor body form.
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
                                        // Returns (field_name, child_annotation) — the annotation
                                        // is kept so the role can be read from it directly.
                                        let (field_name, child_ann) = match &k.expr {
                                            SurfaceExpression::VarRef {
                                                name: fn_,
                                                annotation,
                                                ..
                                            } => {
                                                // Accept @Child (Simple) or @Child@[role: ...] (Annotated
                                                // with Simple("Child") outer). Both declare a child field.
                                                let child_ann = annotation.as_ref().and_then(|ann| {
                                                    let is_child = match &ann.node {
                                                        crate::ast::Annotation::Simple(s) => s == "Child",
                                                        crate::ast::Annotation::Annotated(outer, _) => {
                                                            matches!(outer.as_ref(), crate::ast::Annotation::Simple(s) if s == "Child")
                                                        }
                                                        _ => false,
                                                    };
                                                    if is_child { Some(&ann.node) } else { None }
                                                });
                                                (Some(fn_.clone()), child_ann)
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => (Some(content.clone()), None),
                                            _ => (None, None),
                                        };
                                        if let Some(fn_) = field_name {
                                            if let Some(ann) = child_ann {
                                                let role = child_role_from_annotation(ann);
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
                // Old body form: union of positional unit constructors.
                // Each positional entry is a separate unit constructor (form 3).
                // The old single-constructor dict form (form 5, first positional VarRef + keyed entries)
                // is rejected at parse time by T-1539 and never appears here.
                for entry in entries {
                    if entry.node.key.is_none() {
                        try_extract_one(&entry.node.value.expr, &mut ctors);
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

        let (lowered, diags) = lower(&node, None);

        assert!(diags.is_empty(), "unexpected diagnostics: {:?}", diags);
        match lowered.node {
            CoreExpr::Var { name, addr, .. } => {
                assert_eq!(name, "x");
                // resolution was Dispatch(0, 3) → addr is the same
                assert_eq!(addr, VarAddr::Dispatch(0, 3));
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
            diags[0].level == crate::error::DiagnosticLevel::Err,
            "expected Error diagnostic, got {:?}",
            diags[0].level
        );
        assert!(
            diags[0].message.contains("unbound"),
            "diagnostic message should mention the variable name: {}",
            diags[0].message
        );
    }

    // [type MyType Int] in standalone expression position must lower to an empty dict.
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
                "standalone [type ...] must lower to empty dict, got {} entries",
                entries.len()
            ),
            other => panic!(
                "expected CoreExpr::Dict([]) for standalone TypeAlias, got {:?}",
                other
            ),
        }
    }

    // [type Color Red Green Blue] standalone also lowers to empty dict.
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
                "standalone [type Red Green Blue] must lower to empty dict, got {} entries",
                entries.len()
            ),
            other => panic!(
                "expected CoreExpr::Dict([]) for standalone sum-type TypeAlias, got {:?}",
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
    /// `resolved_captures` is pre-set to an empty capture list so `lower()` does not panic
    /// with "resolved_captures not set". In practice the resolver sets this; in unit tests
    /// we must set it manually before calling `lower()`.
    fn make_trace_fn_node(trace_value: i64, span: crate::ast::Span) -> Arc<SurfaceNode> {
        use crate::ast::{Annotation, CapturesCell, SurfaceEntry, SurfaceParam, TypeAnnotation};

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

        // resolved_captures MUST be pre-set; lower() panics with expect() if not set.
        let captures_cell = CapturesCell::new();
        captures_cell.set(Arc::new(Vec::new()));

        Arc::new(SurfaceNode::new(
            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared: false,
                resolved_captures: captures_cell,
                resolved_return_annotation: TypeAnnotation::new(),
            },
            span,
        ))
    }

    /// T-2142: A function with @[trace: 1] annotation must emit a "trace-lower" Info diagnostic
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

        let (_lowered, diags) = lower(&node, None);

        let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-lower").collect();
        assert!(
            !trace_diags.is_empty(),
            "Expected at least one trace-lower diagnostic for @[trace: 1] fn; got: {:?}",
            diags
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

        let (_lowered, diags) = lower(&node, None);

        let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-lower").collect();
        assert!(
            trace_diags.is_empty(),
            "Expected NO trace-lower diagnostic for @[trace: 0] — guard requires >= 1; got: {:?}",
            diags
        );
    }

    /// T-2142: A function with NO trace annotation must NOT emit a trace-lower diagnostic.
    ///
    /// Clean-path invariant: without this test, a mutation that emits trace-lower unconditionally
    /// would pass `test_trace_annotation_lower_emits_diagnostic` but fail here.
    #[test]
    fn test_no_trace_lower_without_annotation() {
        use crate::ast::{CapturesCell, SurfaceParam, TypeAnnotation};

        let span = rust_span!();

        // Plain function with NO return annotation — return_ann is None.
        let body = Arc::new(SurfaceNode::new(SurfaceExpression::Int(42), span.clone()));
        let params: Vec<Spanned<SurfaceParam>> = Vec::new();
        let captures_cell = CapturesCell::new();
        captures_cell.set(Arc::new(Vec::new()));

        let node = Arc::new(SurfaceNode::new(
            SurfaceExpression::Fn {
                return_ann: None,
                params,
                body,
                desugared: false,
                resolved_captures: captures_cell,
                resolved_return_annotation: TypeAnnotation::new(),
            },
            span,
        ));

        let (_lowered, diags) = lower(&node, None);

        let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-lower").collect();
        assert!(
            trace_diags.is_empty(),
            "Expected zero trace-lower diagnostics for unannotated function; got: {:?}",
            trace_diags
        );
    }
}
