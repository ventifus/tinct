//! Type quality diagnostics: T010/T011/T012 scanning subsystem.
//!
//! This module is completely standalone — it only reads [`TypeMap`] and [`SurfaceProgram`]
//! and emits [`TypeDiagnostic`]s. It does NOT call back into inference.

use std::collections::{HashMap, HashSet};

// ============================================================================
// TypeDiagnostic Code Catalog
// ============================================================================
//
// All valid TypeDiagnostic codes are declared here as const &'static str.
// Use these constants when creating TypeDiagnostic instances to prevent typos.
//
// Legend:
//   T0xx = Type quality and annotation diagnostics (Unknown inference, overbroad annotations,
//          unknown type parameter annotations)
//   T1xx = Constraint/unification ambiguities
//   T2xx = Pattern matching issues
//   W0xx = Warnings (non-error conditions)

/// T010: Inferred type is Unknown — consider adding a type annotation (Warn)
pub const T010_INFERRED_UNKNOWN: &str = "T010";

/// T011: Explicit @Unknown annotation — type is not statically known (Info)
pub const T011_EXPLICIT_UNKNOWN: &str = "T011";

/// T012: Over-broad annotation — inferred type is more specific than declared (Info)
pub const T012_OVERBROAD_ANNOTATION: &str = "T012";

/// T013: Ambiguous constraint — multiple instances match, cannot resolve (Warn)
pub const T013_AMBIGUOUS_CONSTRAINT: &str = "T013";

/// T018: Match pattern type mismatch (context-dependent level)
pub const T018_MATCH_PATTERN_MISMATCH: &str = "T018";

/// T019: Match pattern guard failure (context-dependent level)
pub const T019_MATCH_GUARD_FAILURE: &str = "T019";

/// T020: Match pattern exhaustiveness issue (context-dependent level)
pub const T020_MATCH_EXHAUSTIVENESS: &str = "T020";

/// T021: Unknown type parameter annotation — not a variance keyword or registered class (Warn)
pub const T021_UNKNOWN_TYPE_PARAM_ANNOTATION: &str = "T021";

/// W042: Duplicate nominal variant tags in type definition (Warn)
pub const W042_DUPLICATE_NOMINAL_TAG: &str = "W042";

/// W043: Structural instance overlap — instance declarations overlap (Warn)
pub const W043_INSTANCE_OVERLAP: &str = "W043";

// ============================================================================

use super::TypeMap;
use crate::ast::{
    Annotation, Span, SurfaceDeclaration, SurfaceExpression, SurfaceItem, SurfaceNode,
    SurfaceProgram,
};
use crate::error::{DiagnosticLevel, TypeDiagnostic};
use crate::types::Type;

/// Check if a type recursively contains `Unknown`.
pub(crate) fn stq_contains_unknown(ty: &Type) -> bool {
    match ty {
        Type::Unknown => true,
        Type::Record(row) => row.fields.values().any(stq_contains_unknown),
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, t)| stq_contains_unknown(t)) || stq_contains_unknown(ret)
        }
        Type::App(f, a) => stq_contains_unknown(f) || stq_contains_unknown(a),
        Type::TyCon(_) => false,
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().any(stq_contains_unknown)
        }
        Type::Negation(t) => stq_contains_unknown(t),
        _ => false,
    }
}

/// Check if an annotation explicitly references `Unknown`.
pub(crate) fn stq_is_unknown_annotation(ann: &Annotation) -> bool {
    match ann {
        Annotation::Simple(name) => name == "Unknown",
        Annotation::PropertyDict(entries) => {
            // Check if there's a "return: Unknown" entry (for function metadata dicts)
            entries.iter().any(|entry| {
                if let Some(key_node) = &entry.node.key {
                    if let SurfaceExpression::Str(key_name) = &key_node.expr {
                        if key_name == "return" {
                            if let SurfaceExpression::VarRef { name, .. } = &entry.node.value.expr {
                                return name == "Unknown";
                            }
                        }
                    }
                }
                false
            })
        }
        Annotation::Annotated(_, _) => false,
    }
}

/// Walk a SurfaceNode recursively, collecting spans of explicit `@Unknown` annotations.
pub(crate) fn stq_walk_node_unknown(node: &SurfaceNode, spans: &mut HashSet<(usize, usize)>) {
    match &node.expr {
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            if stq_is_unknown_annotation(&annotation.node) {
                spans.insert((node.span.start.offset, node.span.end.offset));
            }
            stq_walk_node_unknown(inner, spans);
        }
        SurfaceExpression::Fn {
            return_ann, body, ..
        } => {
            if let Some(ann) = return_ann {
                if stq_is_unknown_annotation(&ann.node) {
                    spans.insert((node.span.start.offset, node.span.end.offset));
                }
            }
            stq_walk_node_unknown(body, spans);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            stq_walk_node_unknown(func, spans);
            for arg in args {
                stq_walk_node_unknown(arg, spans);
            }
            for na in named_args {
                stq_walk_node_unknown(&na.node.value, spans);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                stq_walk_node_unknown(e, spans);
            }
        }
        SurfaceExpression::DotAccess { expr, .. } => stq_walk_node_unknown(expr, spans),
        SurfaceExpression::Pipe { lhs, rhs } => {
            stq_walk_node_unknown(lhs, spans);
            stq_walk_node_unknown(rhs, spans);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            stq_walk_node_unknown(scrutinee, spans);
            for arm in arms {
                stq_walk_node_unknown(&arm.body, spans);
                if let Some(guard) = &arm.guard {
                    stq_walk_node_unknown(guard, spans);
                }
            }
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    stq_walk_node_unknown(key, spans);
                }
                stq_walk_node_unknown(&entry.node.value, spans);
            }
        }
        SurfaceExpression::Quote(e)
        | SurfaceExpression::Unquote(e)
        | SurfaceExpression::UnquoteSplice(e) => stq_walk_node_unknown(e, spans),
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            for b in bindings {
                stq_walk_node_unknown(b, spans);
            }
        }
        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            if let Some(lb) = let_bindings {
                stq_walk_node_unknown(lb, spans);
            }
            stq_walk_node_unknown(pattern, spans);
            stq_walk_node_unknown(body, spans);
        }
        SurfaceExpression::Decl(decl) => stq_walk_decl_unknown(decl, spans),
        _ => {}
    }
}

/// Walk a SurfaceDeclaration recursively, collecting spans of explicit `@Unknown` annotations.
pub(crate) fn stq_walk_decl_unknown(
    decl: &SurfaceDeclaration,
    spans: &mut HashSet<(usize, usize)>,
) {
    match decl {
        SurfaceDeclaration::TypeAlias { body, .. } => stq_walk_node_unknown(body, spans),
        SurfaceDeclaration::ClassDecl { methods, .. } => {
            for entry in methods {
                if let Some(key) = &entry.node.key {
                    stq_walk_node_unknown(key, spans);
                }
                stq_walk_node_unknown(&entry.node.value, spans);
            }
        }
        SurfaceDeclaration::InstanceDecl { arms, .. } => {
            for (pattern, methods) in arms {
                stq_walk_node_unknown(pattern, spans);
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        stq_walk_node_unknown(key, spans);
                    }
                    stq_walk_node_unknown(&entry.node.value, spans);
                }
            }
        }
        SurfaceDeclaration::MacroDecl { params, body, .. } => {
            stq_walk_node_unknown(params, spans);
            stq_walk_node_unknown(body, spans);
        }
        SurfaceDeclaration::SyntaxClass { pattern, .. } => stq_walk_node_unknown(pattern, spans),
        SurfaceDeclaration::Splice(forms) => {
            for form in forms {
                stq_walk_node_unknown(form, spans);
            }
        }
    }
}

/// Walk a SurfaceNode recursively, checking for over-broad function return annotations.
pub(crate) fn stq_walk_node_overbroad(
    node: &SurfaceNode,
    type_map: &TypeMap,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &node.expr {
        SurfaceExpression::Fn {
            return_ann: Some(ann),
            body,
            ..
        } => {
            if let Some(declared_type) = stq_resolve_simple_annotation(&ann.node) {
                let body_key = (body.span.start.offset, body.span.end.offset);
                if let Some(inferred_type) = type_map.get(&body_key) {
                    if Type::is_subtype(inferred_type, &declared_type, None)
                        && !Type::is_subtype(&declared_type, inferred_type, None)
                    {
                        let type_str = format!("{}", inferred_type);
                        let ann_str = format!("{}", ann.node);
                        diagnostics.push(TypeDiagnostic {
                            level: DiagnosticLevel::Info,
                            code: T012_OVERBROAD_ANNOTATION,
                            message: format!(
                                "annotation @{} is over-broad — inferred type is {}; consider using @{}",
                                ann_str, type_str, type_str
                            ),
                            span: ann.span.clone(),
                        });
                    }
                }
            }
            stq_walk_node_overbroad(body, type_map, diagnostics);
        }
        SurfaceExpression::Fn { body, .. } => {
            stq_walk_node_overbroad(body, type_map, diagnostics);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            stq_walk_node_overbroad(func, type_map, diagnostics);
            for arg in args {
                stq_walk_node_overbroad(arg, type_map, diagnostics);
            }
            for na in named_args {
                stq_walk_node_overbroad(&na.node.value, type_map, diagnostics);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                stq_walk_node_overbroad(e, type_map, diagnostics);
            }
        }
        SurfaceExpression::DotAccess { expr, .. } => {
            stq_walk_node_overbroad(expr, type_map, diagnostics)
        }
        SurfaceExpression::Pipe { lhs, rhs } => {
            stq_walk_node_overbroad(lhs, type_map, diagnostics);
            stq_walk_node_overbroad(rhs, type_map, diagnostics);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            stq_walk_node_overbroad(scrutinee, type_map, diagnostics);
            for arm in arms {
                stq_walk_node_overbroad(&arm.body, type_map, diagnostics);
                if let Some(guard) = &arm.guard {
                    stq_walk_node_overbroad(guard, type_map, diagnostics);
                }
            }
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    stq_walk_node_overbroad(key, type_map, diagnostics);
                }
                stq_walk_node_overbroad(&entry.node.value, type_map, diagnostics);
            }
        }
        SurfaceExpression::TypeAssert { expr, .. } => {
            stq_walk_node_overbroad(expr, type_map, diagnostics)
        }
        SurfaceExpression::Quote(e)
        | SurfaceExpression::Unquote(e)
        | SurfaceExpression::UnquoteSplice(e) => stq_walk_node_overbroad(e, type_map, diagnostics),
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            for b in bindings {
                stq_walk_node_overbroad(b, type_map, diagnostics);
            }
        }
        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            if let Some(lb) = let_bindings {
                stq_walk_node_overbroad(lb, type_map, diagnostics);
            }
            stq_walk_node_overbroad(pattern, type_map, diagnostics);
            stq_walk_node_overbroad(body, type_map, diagnostics);
        }
        SurfaceExpression::Decl(decl) => stq_walk_decl_overbroad(decl, type_map, diagnostics),
        _ => {}
    }
}

/// Walk a SurfaceDeclaration recursively, checking for over-broad function return annotations.
pub(crate) fn stq_walk_decl_overbroad(
    decl: &SurfaceDeclaration,
    type_map: &TypeMap,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match decl {
        SurfaceDeclaration::TypeAlias { body, .. } => {
            stq_walk_node_overbroad(body, type_map, diagnostics)
        }
        SurfaceDeclaration::ClassDecl { methods, .. } => {
            for entry in methods {
                if let Some(key) = &entry.node.key {
                    stq_walk_node_overbroad(key, type_map, diagnostics);
                }
                stq_walk_node_overbroad(&entry.node.value, type_map, diagnostics);
            }
        }
        SurfaceDeclaration::InstanceDecl { arms, .. } => {
            for (pattern, methods) in arms {
                stq_walk_node_overbroad(pattern, type_map, diagnostics);
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        stq_walk_node_overbroad(key, type_map, diagnostics);
                    }
                    stq_walk_node_overbroad(&entry.node.value, type_map, diagnostics);
                }
            }
        }
        SurfaceDeclaration::MacroDecl { params, body, .. } => {
            stq_walk_node_overbroad(params, type_map, diagnostics);
            stq_walk_node_overbroad(body, type_map, diagnostics);
        }
        SurfaceDeclaration::SyntaxClass { pattern, .. } => {
            stq_walk_node_overbroad(pattern, type_map, diagnostics)
        }
        SurfaceDeclaration::Splice(forms) => {
            for form in forms {
                stq_walk_node_overbroad(form, type_map, diagnostics);
            }
        }
    }
}

/// Resolve a simple annotation name to a concrete Type (for over-broad annotation detection).
pub(crate) fn stq_resolve_simple_annotation(ann: &Annotation) -> Option<Type> {
    match ann {
        Annotation::Simple(name) => match name.as_str() {
            "Int" => Some(Type::Int),
            "Float" => Some(Type::Float),
            "Number" => Some(Type::Number),
            "Str" => Some(Type::Str),
            "Bool" => Some(Type::Bool),
            "Top" => Some(Type::Top),
            "Unknown" => Some(Type::Unknown),
            _ => None,
        },
        Annotation::PropertyDict(_) => None,
        Annotation::Annotated(_, _) => None,
    }
}

/// Walk a SurfaceNode recursively, collecting `(start_offset, end_offset) → Span` for every node.
///
/// This is used by `scan_type_quality` to look up real line/column positions when emitting
/// T010/T011 diagnostics — the TypeMap only stores offset pairs as keys, so we need to recover
/// the full Span (with line/column) from the Surface AST.
pub(crate) fn stq_collect_node_spans(node: &SurfaceNode, map: &mut HashMap<(usize, usize), Span>) {
    let key = (node.span.start.offset, node.span.end.offset);
    map.entry(key).or_insert_with(|| node.span.clone());

    match &node.expr {
        SurfaceExpression::TypeAssert { expr: inner, .. } => {
            stq_collect_node_spans(inner, map);
        }
        SurfaceExpression::Fn { body, .. } => {
            stq_collect_node_spans(body, map);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            stq_collect_node_spans(func, map);
            for arg in args {
                stq_collect_node_spans(arg, map);
            }
            for na in named_args {
                stq_collect_node_spans(&na.node.value, map);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                stq_collect_node_spans(e, map);
            }
        }
        SurfaceExpression::DotAccess { expr, .. } => stq_collect_node_spans(expr, map),
        SurfaceExpression::Pipe { lhs, rhs } => {
            stq_collect_node_spans(lhs, map);
            stq_collect_node_spans(rhs, map);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            stq_collect_node_spans(scrutinee, map);
            for arm in arms {
                stq_collect_node_spans(&arm.body, map);
                if let Some(guard) = &arm.guard {
                    stq_collect_node_spans(guard, map);
                }
            }
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(key_node) = &entry.node.key {
                    stq_collect_node_spans(key_node, map);
                }
                stq_collect_node_spans(&entry.node.value, map);
            }
        }
        SurfaceExpression::Quote(e)
        | SurfaceExpression::Unquote(e)
        | SurfaceExpression::UnquoteSplice(e) => stq_collect_node_spans(e, map),
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            for b in bindings {
                stq_collect_node_spans(b, map);
            }
        }
        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            if let Some(lb) = let_bindings {
                stq_collect_node_spans(lb, map);
            }
            stq_collect_node_spans(pattern, map);
            stq_collect_node_spans(body, map);
        }
        SurfaceExpression::Decl(decl) => stq_collect_decl_spans(decl, map),
        _ => {}
    }
}

/// Walk a SurfaceDeclaration recursively, collecting `(start_offset, end_offset) → Span`.
pub(crate) fn stq_collect_decl_spans(
    decl: &SurfaceDeclaration,
    map: &mut HashMap<(usize, usize), Span>,
) {
    match decl {
        SurfaceDeclaration::TypeAlias { body, .. } => stq_collect_node_spans(body, map),
        SurfaceDeclaration::ClassDecl { methods, .. } => {
            for entry in methods {
                if let Some(key_node) = &entry.node.key {
                    stq_collect_node_spans(key_node, map);
                }
                stq_collect_node_spans(&entry.node.value, map);
            }
        }
        SurfaceDeclaration::InstanceDecl { arms, .. } => {
            for (pattern, methods) in arms {
                stq_collect_node_spans(pattern, map);
                for entry in methods {
                    if let Some(key_node) = &entry.node.key {
                        stq_collect_node_spans(key_node, map);
                    }
                    stq_collect_node_spans(&entry.node.value, map);
                }
            }
        }
        SurfaceDeclaration::MacroDecl { params, body, .. } => {
            stq_collect_node_spans(params, map);
            stq_collect_node_spans(body, map);
        }
        SurfaceDeclaration::SyntaxClass { pattern, .. } => stq_collect_node_spans(pattern, map),
        SurfaceDeclaration::Splice(forms) => {
            for form in forms {
                stq_collect_node_spans(form, map);
            }
        }
    }
}

/// Emit T011 diagnostics for explicit `@Unknown` annotations without requiring a type_map.
///
/// Used when `enable_scheme_map = false` (the normal eval path) so that T011 fires
/// for explicitly-annotated `@Unknown` even though the type_map is not populated.
/// When `enable_scheme_map = true`, `scan_type_quality` already handles T011 via the
/// type_map; this function is skipped to avoid duplicates.
///
/// This walker fires T011 unconditionally for any `@Unknown` annotation in the source:
/// `[fn@Unknown ...]`, `[@Unknown expr]`, etc. No inferred-type lookup is needed.
pub(crate) fn scan_explicit_unknown_t011(
    ast: &SurfaceProgram,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    fn emit_t011_for_node(node: &SurfaceNode, diagnostics: &mut Vec<TypeDiagnostic>) {
        match &node.expr {
            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
            } => {
                if stq_is_unknown_annotation(&annotation.node) {
                    diagnostics.push(TypeDiagnostic {
                        level: DiagnosticLevel::Info,
                        code: T011_EXPLICIT_UNKNOWN,
                        message: "explicit @Unknown annotation — type is not statically known"
                            .to_string(),
                        span: node.span.clone(),
                    });
                }
                emit_t011_for_node(inner, diagnostics);
            }
            SurfaceExpression::Fn {
                return_ann, body, ..
            } => {
                if let Some(ann) = return_ann {
                    if stq_is_unknown_annotation(&ann.node) {
                        diagnostics.push(TypeDiagnostic {
                            level: DiagnosticLevel::Info,
                            code: T011_EXPLICIT_UNKNOWN,
                            message: "explicit @Unknown annotation — type is not statically known"
                                .to_string(),
                            span: node.span.clone(),
                        });
                    }
                }
                emit_t011_for_node(body, diagnostics);
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                emit_t011_for_node(func, diagnostics);
                for arg in args {
                    emit_t011_for_node(arg, diagnostics);
                }
                for na in named_args {
                    emit_t011_for_node(&na.node.value, diagnostics);
                }
            }
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    emit_t011_for_node(e, diagnostics);
                }
            }
            SurfaceExpression::DotAccess { expr, .. } => emit_t011_for_node(expr, diagnostics),
            SurfaceExpression::Pipe { lhs, rhs } => {
                emit_t011_for_node(lhs, diagnostics);
                emit_t011_for_node(rhs, diagnostics);
            }
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        emit_t011_for_node(key, diagnostics);
                    }
                    emit_t011_for_node(&entry.node.value, diagnostics);
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                emit_t011_for_node(scrutinee, diagnostics);
                for arm in arms {
                    emit_t011_for_node(&arm.body, diagnostics);
                    if let Some(guard) = &arm.guard {
                        emit_t011_for_node(guard, diagnostics);
                    }
                }
            }
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => emit_t011_for_node(e, diagnostics),
            SurfaceExpression::PatternDecl { bindings }
            | SurfaceExpression::LetDecl { bindings } => {
                for b in bindings {
                    emit_t011_for_node(b, diagnostics);
                }
            }
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                if let Some(lb) = let_bindings {
                    emit_t011_for_node(lb, diagnostics);
                }
                emit_t011_for_node(pattern, diagnostics);
                emit_t011_for_node(body, diagnostics);
            }
            SurfaceExpression::Decl(decl) => emit_t011_for_decl(decl, diagnostics),
            _ => {}
        }
    }

    fn emit_t011_for_decl(decl: &SurfaceDeclaration, diagnostics: &mut Vec<TypeDiagnostic>) {
        match decl {
            SurfaceDeclaration::TypeAlias { body, .. } => emit_t011_for_node(body, diagnostics),
            SurfaceDeclaration::ClassDecl { methods, .. } => {
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        emit_t011_for_node(key, diagnostics);
                    }
                    emit_t011_for_node(&entry.node.value, diagnostics);
                }
            }
            SurfaceDeclaration::InstanceDecl { arms, .. } => {
                for (pattern, methods) in arms {
                    emit_t011_for_node(pattern, diagnostics);
                    for entry in methods {
                        if let Some(key) = &entry.node.key {
                            emit_t011_for_node(key, diagnostics);
                        }
                        emit_t011_for_node(&entry.node.value, diagnostics);
                    }
                }
            }
            SurfaceDeclaration::MacroDecl { params, body, .. } => {
                emit_t011_for_node(params, diagnostics);
                emit_t011_for_node(body, diagnostics);
            }
            SurfaceDeclaration::SyntaxClass { pattern, .. } => {
                emit_t011_for_node(pattern, diagnostics);
            }
            SurfaceDeclaration::Splice(forms) => {
                for form in forms {
                    emit_t011_for_node(form, diagnostics);
                }
            }
        }
    }

    for doc_spanned in &ast.documents {
        for item in &doc_spanned.node.items {
            match item {
                SurfaceItem::Expr(node) => emit_t011_for_node(node, diagnostics),
                SurfaceItem::Decl(decl_spanned) => {
                    emit_t011_for_decl(&decl_spanned.node, diagnostics)
                }
            }
        }
    }
}

/// Scan for type quality issues (Unknown types, over-broad annotations).
///
/// Emits diagnostics at base level (Info/Warn). In `--strict` mode the CLI bumps
/// each diagnostic's level via `DiagnosticLevel::bump()` and treats any resulting
/// `Err`-level diagnostic as fatal (see `main.rs` run/fmt/lint handlers).
/// This is called at the end of type checking to produce advisory notifications.
///
/// Accepts `&SurfaceProgram` — walks the Surface AST natively via `SurfaceExpression`.
pub(crate) fn scan_type_quality(
    type_map: &TypeMap,
    ast: &SurfaceProgram,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    // Build a map from (start_offset, end_offset) → full Span (with real line/column).
    // The TypeMap uses offset pairs as keys; this allows us to recover line/column for display.
    let mut span_map: HashMap<(usize, usize), Span> = HashMap::new();
    for doc_spanned in &ast.documents {
        for item in &doc_spanned.node.items {
            match item {
                SurfaceItem::Expr(node) => stq_collect_node_spans(node, &mut span_map),
                SurfaceItem::Decl(decl_spanned) => {
                    stq_collect_decl_spans(&decl_spanned.node, &mut span_map)
                }
            }
        }
    }

    // Collect all explicit @Unknown annotation spans from the Surface AST.
    let mut explicit_unknown_spans: HashSet<(usize, usize)> = HashSet::new();
    for doc_spanned in &ast.documents {
        for item in &doc_spanned.node.items {
            match item {
                SurfaceItem::Expr(node) => stq_walk_node_unknown(node, &mut explicit_unknown_spans),
                SurfaceItem::Decl(decl_spanned) => {
                    stq_walk_decl_unknown(&decl_spanned.node, &mut explicit_unknown_spans)
                }
            }
        }
    }

    // Scan all inferred types for Unknown
    for ((start, end), ty) in type_map {
        if stq_contains_unknown(ty) {
            let is_explicit = explicit_unknown_spans.contains(&(*start, *end));

            let (level, code, message) = if is_explicit {
                (
                    DiagnosticLevel::Info,
                    T011_EXPLICIT_UNKNOWN,
                    "explicit @Unknown annotation — type is not statically known".to_string(),
                )
            } else {
                (
                    DiagnosticLevel::Warn,
                    T010_INFERRED_UNKNOWN,
                    "inferred type is Unknown — consider adding a type annotation".to_string(),
                )
            };

            // Use the real Span (with line/column) from the span map when available.
            // Fall back to an offset-only span if the node was not found in the walk
            // (e.g., synthetic nodes introduced during type inference).
            let span = span_map.get(&(*start, *end)).cloned().unwrap_or(Span {
                start: crate::ast::Position {
                    offset: *start,
                    line: 0,
                    column: 0,
                },
                end: crate::ast::Position {
                    offset: *end,
                    line: 0,
                    column: 0,
                },
                file: None,
            });

            diagnostics.push(TypeDiagnostic {
                level,
                code,
                message,
                span,
            });
        }
    }

    // Over-broad annotation detection (Tasks 3 & 4)
    check_overbroad_annotations(ast, type_map, diagnostics);
}

/// Check for over-broad annotations where the declared type is wider than inferred.
///
/// Detects patterns like:
/// - `fn@Number` when body infers `Int` → suggest `@Int`
/// - `fn@Top` when body infers a specific type → suggest the specific type
///
/// Accepts `&SurfaceProgram` — walks the Surface AST natively via `SurfaceExpression`.
pub(crate) fn check_overbroad_annotations(
    ast: &SurfaceProgram,
    type_map: &TypeMap,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    for doc_spanned in &ast.documents {
        for item in &doc_spanned.node.items {
            match item {
                SurfaceItem::Expr(node) => stq_walk_node_overbroad(node, type_map, diagnostics),
                SurfaceItem::Decl(decl_spanned) => {
                    stq_walk_decl_overbroad(&decl_spanned.node, type_map, diagnostics)
                }
            }
        }
    }
}
