//! Type annotation resolution and type expression parsing.
//!
//! After the S-1003 runtime-types migration, all type-level values are represented as
//! `Arc<Value>` (TypeValue) with TypeValue.* constructor tags rather than the `Type` enum.
//! The `Type` enum has been deleted; this file works with TypeValue throughout.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::error::Diagnostic;
use crate::rust_span;
use crate::type_class::TypeValue;
use crate::type_def::TyConDef;
use crate::type_infer::{
    make_rowtail_uniform, make_rowtail_uniform_with_key_type, make_typevalue_app,
    make_typevalue_float_lit, make_typevalue_fn, make_typevalue_fn_with_flags,
    make_typevalue_int_lit, make_typevalue_negation, make_typevalue_never, make_typevalue_nominal,
    make_typevalue_nominal_variant, make_typevalue_record, make_typevalue_recursive,
    make_typevalue_str_lit, make_typevalue_top, make_typevalue_unknown, make_typevalue_var,
    typevalue_normalize_intersection, typevalue_normalize_union, typevalue_normalize_union_ctx,
};
use crate::type_tags::*;
use crate::types::InferState;
// Constraint is now Vec<Arc<Value>> — no Constraint type import needed.
// Kind is deleted — kind names are now plain strings ("Type", "Operator", "Label").
use crate::value::{HashableValue, Thunk, Value, TYPE_ID_TYPE_NODE, TYPE_ID_TYPE_VALUE};

const KIND: &str = "type-check";

/// Protocol name for the type-stage TypeNode dispatch function (Axiom 1 / D-7).
///
/// Any compliant prelude must export a type-stage function under this exact name that
/// accepts a TypeNode value and returns its resolved Type.  Rust defines the protocol;
/// the prelude implements it.  See `as_type_dispatch` for usage.
pub(crate) const AS_TYPENODE_PROTOCOL_NAME: &str = "as-typenode";

/// Resolve a function metadata dict `fn@[return: ... constraint: ... doc: ...]`.
///
/// Processes keys in fixed order:
/// 0. `bind:` — declares TypeVars in ann_mapping (processed before this function is called)
/// 1. `kinds:` — registers kind constraints on declared TypeVars
/// 2. `constraint:` keyed entries — single-param class constraints (e.g., `[a: Comparable]`)
/// 3. `constraint:` MPTC positional entries — multi-param class constraints (e.g., `[$Add a b c]`)
/// 4. `return:` — resolves via resolve_type_expr (may reference TypeVars from bind:/constraint:)
/// 5. `doc:` — extracts string literal, returned as Option<String>
///
/// Returns (return_type, doc_string).
pub(crate) async fn resolve_fn_metadata(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<(TypeValue, Option<String>), Diagnostic> {
    let mut return_type: Option<TypeValue> = None;
    let mut doc_string: Option<String> = None;

    // Step 0: Process bind: entries (must come first so TypeVars exist for return:/constraint:/kinds:)
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "bind" {
                    // bind: [a b c] — positional list of TypeVar names.
                    //
                    // The LLT parser represents `[a b c]` (three bare names) as
                    // SurfaceExpression::Call (call `a` with args `b`, `c`), and `[a]`
                    // (one bare name) as SurfaceExpression::Call (zero-arg call to `a`).
                    // We accept Call-form bind lists by treating the function name and each
                    // positional arg as a TypeVar name to bind. The Dict form is also accepted
                    // for compatibility, though the parser does not produce it for bare-name
                    // lists.
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict {
                            entries: bind_entries,
                            scope: _,
                        } => {
                            for bind_entry in bind_entries {
                                if bind_entry.node.key.is_some() {
                                    return Err(Diagnostic::error(KIND,
                                        "bind: list must contain only positional entries (bare names)".to_string(),
                                        bind_entry.span.clone(),
                                    ));
                                }
                                match &bind_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Check lowercase convention for TypeVar names
                                        if !name.starts_with(|c: char| c.is_lowercase()) {
                                            return Err(Diagnostic::error(KIND,
                                                format!(
                                                    "bind: TypeVar name '{}' must start with lowercase letter",
                                                    name
                                                ),
                                                bind_entry.node.value.span.clone(),
                                            ));
                                        }
                                        // Create fresh TypeVar and register in ann_mapping
                                        let level = state.ctx.current_level;
                                        let fresh = state
                                            .fresh_type_var_with(
                                                Some(name.as_str()),
                                                Some(level),
                                                "Type",
                                                &bind_entry.node.value.span,
                                            )
                                            .0;
                                        // Mark as protected: bind: TypeVars are explicitly
                                        // declared polymorphic type parameters. They must not
                                        // be level-lowered by constrain(Unknown, α) — that
                                        // would prevent generalization (B-681).
                                        state.ctx.protected_vars.insert(fresh.clone());
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), name.clone());
                                        if let Some(ref mut mapping) = ann_mapping {
                                            mapping.insert(name.clone(), fresh);
                                        } else {
                                            return Err(Diagnostic::error(
                                                KIND,
                                                "bind: requires an annotation mapping context"
                                                    .to_string(),
                                                span,
                                            ));
                                        }
                                    }
                                    _ => {
                                        return Err(Diagnostic::error(
                                            KIND,
                                            "bind: entries must be bare names (TypeVar names)"
                                                .to_string(),
                                            bind_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        // Call form: `[a b c]` is parsed as
                        // Call(VarRef("a"), [VarRef("b"), VarRef("c")]).
                        // `[a]` (single-element) is Call(VarRef("a"), []) — a zero-arg call.
                        // Treat func + each positional arg as the ordered list of TypeVar names.
                        SurfaceExpression::Call {
                            func,
                            args,
                            named_args,
                            ..
                        } => {
                            if !named_args.is_empty() {
                                return Err(Diagnostic::error(
                                    KIND,
                                    "bind: list must contain only bare names, not named arguments"
                                        .to_string(),
                                    entry.node.value.span.clone(),
                                ));
                            }
                            // Collect all names: func first, then each positional arg
                            let all_names: Vec<(&str, Span)> =
                                {
                                    let mut v: Vec<(&str, Span)> = Vec::new();
                                    match &func.expr {
                                        SurfaceExpression::VarRef { name, .. } => {
                                            v.push((name.as_str(), func.span.clone()))
                                        }
                                        _ => {
                                            return Err(Diagnostic::error(
                                                KIND,
                                                "bind: entries must be bare names (TypeVar names)",
                                                func.span.clone(),
                                            ))
                                        }
                                    }
                                    for arg in args.iter() {
                                        match &arg.expr {
                                            SurfaceExpression::VarRef { name, .. } => {
                                                v.push((name.as_str(), arg.span.clone()))
                                            }
                                            _ => return Err(Diagnostic::error(
                                                KIND,
                                                "bind: entries must be bare names (TypeVar names)"
                                                    .to_string(),
                                                arg.span.clone(),
                                            )),
                                        }
                                    }
                                    v
                                };
                            for (name, name_span) in all_names {
                                if !name.starts_with(|c: char| c.is_lowercase()) {
                                    return Err(Diagnostic::error(KIND,
                                        format!(
                                            "bind: TypeVar name '{}' must start with lowercase letter",
                                            name
                                        ),
                                        name_span,
                                    ));
                                }
                                let level = state.ctx.current_level;
                                let fresh = state
                                    .fresh_type_var_with(
                                        Some(name),
                                        Some(level),
                                        "Type",
                                        &name_span,
                                    )
                                    .0;
                                // Mark as protected: bind: TypeVars are explicitly
                                // declared polymorphic type parameters (B-681).
                                state.ctx.protected_vars.insert(fresh.clone());
                                // Register source name for better T013 diagnostics
                                state
                                    .type_var_source_names
                                    .insert(fresh.clone(), name.to_string());
                                if let Some(ref mut mapping) = ann_mapping {
                                    mapping.insert(name.to_string(), fresh);
                                } else {
                                    return Err(Diagnostic::error(
                                        KIND,
                                        "bind: requires an annotation mapping context",
                                        span,
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(Diagnostic::error(
                                KIND,
                                "bind: value must be a list [a b c]".to_string(),
                                entry.node.value.span.clone(),
                            ))
                        }
                    }
                }
            }
        }
    }

    // Step 0b: Process kinds: entries (after bind:, so we can validate names exist)
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "kinds" {
                    // kinds: [f: Operator key: Label] — dict mapping TypeVar names to kinds
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict {
                            entries: kinds_entries,
                            scope: _,
                        } => {
                            for kind_entry in kinds_entries {
                                let typevar_name = match &kind_entry.node.key {
                                    Some(k) => {
                                        match &k.expr {
                                            SurfaceExpression::StringLiteral {
                                                content: s, ..
                                            } => s.clone(),
                                            _ => return Err(Diagnostic::error(
                                                KIND,
                                                "kinds: keys must be bare words (TypeVar names)",
                                                kind_entry.span.clone(),
                                            )),
                                        }
                                    }
                                    None => {
                                        return Err(Diagnostic::error(
                                            KIND,
                                            "kinds: entries must be keyed [name: kind]".to_string(),
                                            kind_entry.span.clone(),
                                        ))
                                    }
                                };

                                // Validate that this name was declared in bind:
                                if let Some(ref mapping) = ann_mapping {
                                    if !mapping.contains_key(&typevar_name) {
                                        return Err(Diagnostic::error(
                                            KIND,
                                            format!(
                                                "kinds: TypeVar '{}' not found in bind: list",
                                                typevar_name
                                            ),
                                            kind_entry.span.clone(),
                                        ));
                                    }
                                } else {
                                    return Err(Diagnostic::error(
                                        KIND,
                                        "kinds: requires an annotation mapping context",
                                        span,
                                    ));
                                }

                                // Validate the kind name — Operator and Label are the only valid kinds.
                                // Kind tracking is via fresh_type_var_with(..., kind, ...) at var creation time.
                                // After S-1003: kind_env and set_kind are deleted; no registration needed here.
                                // type_var was used earlier to create the fresh var; the kind is
                                // already encoded in the var's creation-time metadata.
                                match &kind_entry.node.value.expr {
                                    SurfaceExpression::VarRef {
                                        name: kind_name, ..
                                    } if matches!(kind_name.as_str(), "Operator" | "Label") => {}
                                    SurfaceExpression::VarRef {
                                        name: kind_name, ..
                                    } => {
                                        return Err(Diagnostic::error(
                                            KIND,
                                            format!(
                                                "unknown kind '{}' (valid: Operator, Label)",
                                                kind_name
                                            ),
                                            kind_entry.node.value.span.clone(),
                                        ));
                                    }
                                    _ => {
                                        return Err(Diagnostic::error(
                                            KIND,
                                            "kinds: value must be a kind name (Operator or Label)"
                                                .to_string(),
                                            kind_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(Diagnostic::error(
                                KIND,
                                "kinds: value must be a dict [name: kind ...]".to_string(),
                                entry.node.value.span.clone(),
                            ))
                        }
                    }
                }
            }
        }
    }

    // Step 1a: Process constraint: keyed entries (single-param class constraints)
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "constraint" {
                    // constraint: [a: Comparable] or [a: [each Comparable Printable]]
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict {
                            entries: constraint_entries,
                            scope: _,
                        } => {
                            for c_entry in constraint_entries {
                                // Skip positional entries (MPTC) — handled in Step 1b
                                if c_entry.node.key.is_none() {
                                    continue;
                                }

                                let typevar_name = match &c_entry.node.key {
                                    Some(k) => match &k.expr {
                                        SurfaceExpression::StringLiteral { content: s, .. } => {
                                            s.clone()
                                        }
                                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                                        _ => {
                                            return Err(Diagnostic::error(
                                                KIND,
                                                "constraint key must be a bare word (TypeVar name)",
                                                c_entry.span.clone(),
                                            ));
                                        }
                                    },
                                    None => unreachable!(), // already checked above
                                };

                                // Create or get the TypeVar for this name
                                let type_var = if let Some(ref mut mapping) = ann_mapping {
                                    if let Some(existing_var) = mapping.get(&typevar_name) {
                                        existing_var.clone()
                                    } else {
                                        let level = state.ctx.current_level;
                                        let fresh = state
                                            .fresh_type_var_with(
                                                Some(typevar_name.as_str()),
                                                Some(level),
                                                "Type",
                                                &c_entry.span,
                                            )
                                            .0;
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), typevar_name.clone());
                                        mapping.insert(typevar_name.clone(), fresh.clone());
                                        fresh
                                    }
                                } else {
                                    return Err(Diagnostic::error(KIND, "constraint annotations require an annotation mapping context".to_string(),
                                        span,
                                    ));
                                };

                                // Parse the class name(s) — can be a single name or [each ...].
                                // The parser produces Call for bracket forms with bare names:
                                // `[each Comparable Printable]` →
                                // Call { func: VarRef("each"), args: [VarRef("Comparable"), VarRef("Printable")] }.
                                match &c_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Single class: [a: Comparable]
                                        // Unknown class names are deferred — instance resolution
                                        // will report an error if no instance satisfies the constraint.
                                        state.add_constraint_to(
                                            constraints,
                                            name.clone(),
                                            type_var.clone(),
                                        );
                                    }
                                    // Call form: `[each Comparable Printable]` →
                                    // Call(VarRef("each"), [VarRef("Comparable"), VarRef("Printable")]).
                                    // The parser produces Call for bracket forms with bare names.
                                    // If func is "each" (the multi-class keyword), args are the classes.
                                    // If func is a class name with no args, treat as single class.
                                    SurfaceExpression::Call {
                                        func,
                                        args,
                                        named_args,
                                        ..
                                    } => {
                                        if !named_args.is_empty() {
                                            return Err(Diagnostic::error(KIND,
                                                "constraint class list must not contain named arguments".to_string(),
                                                c_entry.node.value.span.clone(),
                                            ));
                                        }
                                        // Determine class names to add
                                        let class_names: Vec<(&str, Span)> =
                                            match &func.expr {
                                                SurfaceExpression::VarRef { name, .. }
                                                    if name == "each" =>
                                                {
                                                    // [each Cls1 Cls2 ...]: args are the class names
                                                    let mut names: Vec<(&str, Span)> = Vec::new();
                                                    for arg in args.iter() {
                                                        match &arg.expr {
                                                            SurfaceExpression::VarRef { name, .. } => {
                                                                names.push((name.as_str(), arg.span.clone()))
                                                            }
                                                            _ => {
                                                                return Err(Diagnostic::error(KIND,
                                                                    "constraint class must be a class name (e.g., Comparable)".to_string(),
                                                                    arg.span.clone(),
                                                                ))
                                                            }
                                                        }
                                                    }
                                                    names
                                                }
                                                SurfaceExpression::VarRef { name, .. }
                                                    if args.is_empty() =>
                                                {
                                                    // [ClassName]: zero-arg call, treat func as single class
                                                    vec![(name.as_str(), func.span.clone())]
                                                }
                                                _ => {
                                                    return Err(Diagnostic::error(KIND,
                                                        "constraint value must be a class name or [each Class1 Class2 ...]".to_string(),
                                                        c_entry.node.value.span.clone(),
                                                    ))
                                                }
                                            };
                                        for (name, _name_span) in class_names {
                                            // Unknown class names deferred to instance resolution.
                                            state.add_constraint_to(
                                                constraints,
                                                name.to_string(),
                                                type_var.clone(),
                                            );
                                        }
                                    }
                                    _ => {
                                        return Err(Diagnostic::error(KIND,
                                            "constraint value must be a class name or list of class names".to_string(),
                                            c_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(Diagnostic::error(
                                KIND,
                                "constraint: value must be a dict [a: Comparable]".to_string(),
                                entry.node.value.span.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Step 1b: Process constraint: MPTC positional entries (multi-param class constraints)
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "constraint" {
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict {
                            entries: constraint_entries,
                            scope: _,
                        } => {
                            let mut i = 0;
                            while i < constraint_entries.len() {
                                let c_entry = &constraint_entries[i];

                                // Only process positional entries (MPTC)
                                if c_entry.node.key.is_some() {
                                    i += 1;
                                    continue;
                                }

                                // MPTC: [$Add a b c] — first positional entry is escaped class name
                                match &c_entry.node.value.expr {
                                    SurfaceExpression::VarRef { escaped, name, .. } if *escaped => {
                                        // Escaped reference like $Add — this is the class name
                                        let class_name = name;

                                        let class_decl = state
                                            .classes
                                            .iter()
                                            .find(|c| c.name == *class_name)
                                            .cloned()
                                            .ok_or_else(|| {
                                                Diagnostic::error(
                                                    KIND,
                                                    format!(
                                                        "undefined class '{}' in constraint",
                                                        class_name
                                                    ),
                                                    c_entry.node.value.span.clone(),
                                                )
                                            })?;

                                        // Collect TypeVar names from subsequent positional entries
                                        let mut typevar_names = Vec::new();
                                        let mut j = i + 1;
                                        while j < constraint_entries.len() {
                                            let subsequent = &constraint_entries[j];
                                            if subsequent.node.key.is_some() {
                                                // Hit a keyed entry — stop collecting
                                                break;
                                            }
                                            match &subsequent.node.value.expr {
                                                SurfaceExpression::VarRef {
                                                    name: var_name,
                                                    escaped: false,
                                                    ..
                                                } => {
                                                    // Validate that this TypeVar is declared in bind:
                                                    if let Some(ref mapping) = ann_mapping {
                                                        if !mapping.contains_key(var_name) {
                                                            return Err(Diagnostic::error(KIND,
                                                                format!(
                                                                    "TypeVar '{}' not declared in bind: — add bind: [{}] before constraint:",
                                                                    var_name, var_name
                                                                ),
                                                                subsequent.node.value.span.clone(),
                                                            ));
                                                        }
                                                        // Map to the actual TypeVar name (e.g., _t0)
                                                        let actual_var =
                                                            mapping.get(var_name).unwrap().clone();
                                                        typevar_names.push(actual_var);
                                                    } else {
                                                        return Err(Diagnostic::error(KIND,
                                                            "constraint annotations require an annotation mapping context".to_string(),
                                                            span,
                                                        ));
                                                    }
                                                }
                                                SurfaceExpression::VarRef {
                                                    escaped: true, ..
                                                } => {
                                                    // Another escaped ref — start of the next MPTC
                                                    break;
                                                }
                                                _ => {
                                                    return Err(Diagnostic::error(KIND,
                                                        "MPTC constraint entries after class name must be TypeVar names".to_string(),
                                                        subsequent.node.value.span.clone(),
                                                    ));
                                                }
                                            }
                                            j += 1;
                                        }

                                        // Build MPTC ConstraintDecl using the new Arc<Value> API (T-1988).
                                        let class_op =
                                            crate::type_class::make_type_op(&class_decl.name);
                                        let constraint_args: Vec<crate::type_class::TypeValue> =
                                            typevar_names
                                                .into_iter()
                                                .map(|n| make_typevalue_var(&n))
                                                .collect();
                                        let constraint_tv = crate::type_class::make_constraint_decl(
                                            class_op,
                                            constraint_args,
                                        );
                                        constraints.push(constraint_tv);

                                        // Skip the entries we just processed (the class name + TypeVars)
                                        i = j;
                                    }
                                    _ => {
                                        // Non-escaped positional entry that's not part of an MPTC
                                        // This is probably an error — bare positional TypeVar names
                                        // without a class
                                        return Err(Diagnostic::error(KIND,
                                            "positional constraint entries must start with escaped class name (e.g., $Add)".to_string(),
                                            c_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            // Already handled in Step 1a
                        }
                    }
                }
            }
        }
    }

    // Step 2: Process return: entry
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "return" {
                    let ret = resolve_type_expr(
                        &entry.node.value,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await?;
                    return_type = Some(ret);
                }
            }
        }
    }

    // Step 3: Process doc: entry
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if key_name == "doc" {
                    // Accept both plain strings and unindent(...) calls (from triple-quoted strings).
                    // Triple-quoted strings `"""..."""` are desugared by the parser to
                    // `Call { func: VarRef("unindent"), args: [Str(s)] }`.
                    let extracted = match &entry.node.value.expr {
                        SurfaceExpression::StringLiteral { content: s, .. } => Some(s.clone()),
                        SurfaceExpression::Call { func, args, .. } => {
                            if matches!(&func.expr,
                                SurfaceExpression::VarRef { name, .. } if name == "unindent")
                            {
                                args.iter().find_map(|arg| {
                                    if let SurfaceExpression::StringLiteral { content: s, .. } =
                                        &arg.expr
                                    {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                })
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    match extracted {
                        Some(s) => doc_string = Some(s),
                        None => {
                            return Err(Diagnostic::error(
                                KIND,
                                "doc: value must be a string literal".to_string(),
                                entry.node.value.span.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Warn about unknown fn annotation keys.
    // Valid keys: return, constraint, doc, bind, kinds.
    const VALID_FN_ANNOTATION_KEYS: &[&str] = &["return", "constraint", "doc", "bind", "kinds"];
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if !VALID_FN_ANNOTATION_KEYS.contains(&key_name.as_str()) {
                    state.diagnostics.push(crate::error::Diagnostic::warn(
                        KIND,
                        format!(
                            "unknown function annotation key '{}' (valid keys: {})",
                            key_name,
                            VALID_FN_ANNOTATION_KEYS.join(", ")
                        ),
                        key_expr.span.clone(),
                    ));
                }
            }
        }
    }

    // If no return: key, default to Unknown (infer from body)
    let ret = return_type.unwrap_or_else(make_typevalue_unknown);

    Ok((ret, doc_string))
}

/// Resolve a bare `Fn@ReturnType` annotation (without parameter list) into a function type.
/// `Fn@T` bare = zero-param function returning T; full function type with params uses `try_resolve_fn_type_expr`.
///
/// For `fn@[...]` PropertyDict annotations, dispatches to:
/// - `resolve_fn_metadata()` if ANY entry has a named key matching `return:`, `constraint:`, or `doc:`
/// - existing union return type path if ALL entries are positional
/// - error if mixed named + positional
async fn resolve_fn_type(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    let surface_entries: Option<&Vec<Spanned<SurfaceEntry>>> = match ann {
        Annotation::Expr(node) => match &node.expr {
            SurfaceExpression::Dict { entries, .. } => Some(entries),
            _ => None,
        },
        Annotation::Quote => None,
    };
    match surface_entries {
        Some(surface_entries) => {
            // Dispatch: if any entry has a named key matching function metadata keys
            // (return:, constraint:, doc:, bind:, kinds:), treat as fn metadata dict.
            // If all entries are positional, delegate to resolve_type_dict (handles
            // [Fn@Return [Params]] and union-style type expressions).
            // `doc:` alone is ambiguous — it could be a record field name.
            // Only treat as fn metadata when a structural key (return/bind/constraint/kinds)
            // is present. `doc:` piggybacks on those.
            const FN_STRUCTURAL_KEYS: &[&str] = &["return", "constraint", "bind", "kinds"];
            let has_fn_key = surface_entries.iter().any(|e| {
                if let Some(ref key) = e.node.key {
                    matches!(&key.expr,
                        SurfaceExpression::StringLiteral { content: s, .. }
                            if FN_STRUCTURAL_KEYS.contains(&s.as_str()))
                } else {
                    false
                }
            });
            // Check if all entries are keyed (no positional entries)
            let all_keyed = surface_entries.iter().all(|e| e.node.key.is_some());

            // Validate that all keyed entries have string (bare-word) keys.
            // Integer keys (e.g., @[42: Int]) are not valid type annotation syntax.
            for entry in surface_entries.iter() {
                if let Some(k) = &entry.node.key {
                    match &k.expr {
                        SurfaceExpression::StringLiteral { .. }
                        | SurfaceExpression::VarRef { .. } => {}
                        _ => {
                            return Err(Diagnostic::error(
                                "invalid-annotation",
                                "property dict annotation requires string (bare-word) keys",
                                k.span.clone(),
                            ));
                        }
                    }
                }
            }

            if has_fn_key {
                // A structural fn-metadata key (return/bind/constraint/kinds) is present,
                // so this is a fn metadata dict.
                if !all_keyed {
                    return Err(Diagnostic::error(KIND,
                        "fn annotation must use either named keys (return:, constraint:, doc:, bind:, kinds:) or positional entries (union return type), not both",
                        span,
                    ));
                }
                let (ret, _doc) = resolve_fn_metadata(
                    surface_entries,
                    span.clone(),
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await?;
                // fn type with zero params: TypeValue.Fn { params: [], return: ret }
                Ok(make_typevalue_fn(vec![], ret))
            } else {
                // All-positional or record-field style — delegate to resolve_type_dict.
                // Handles [Fn@Return [Params]] (detected by try_resolve_fn_type_expr),
                // record types, and type constructors.
                resolve_type_dict(
                    surface_entries,
                    span,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    TypeDictCtx {
                        type_params_scope,
                        tycon_name: "",
                    },
                )
                .await
            }
        }
        None => {
            // Bare-name / Quote / chained-application path: fn@Int, fn@a, fn@Seq@Int, etc.
            let ret = resolve_annotation_as_type(
                ann,
                span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await?;
            // fn type with zero params: TypeValue.Fn { params: [], return: ret }
            Ok(make_typevalue_fn(vec![], ret))
        }
    }
}

/// Shared handling for chained type application — formerly the separate
/// `Annotated(outer, inner)` variant (`Seq@Int`, `@[key:val]@Int`). The only place this
/// logic lives; both `resolve_annotation` and `resolve_annotation_as_type` delegate here
/// via `Annotation::as_application` instead of each maintaining their own copy.
async fn resolve_application(
    outer: &Annotation,
    inner: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    // @Outer@Inner means: apply type constructor `outer` to type argument `inner`.
    // One path for all types — resolve the argument, then delegate to resolve_type_head
    // which handles TyCons, type-stage functions, classes, and kind constructors uniformly.
    let arg = Box::pin(resolve_annotation(
        inner,
        span.clone(),
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
    ))
    .await?;
    // When outer is a bare name (the common case: Seq@Int, Map@[K:V]), call
    // resolve_type_head directly. For non-bare-name outers (PropertyDict@Inner), resolve
    // the outer to a type and produce App(outer_ty, arg).
    match outer.bare_name() {
        Some(name) => Box::pin(resolve_type_head(name, &[arg], state, constraints, span)).await,
        None => {
            let outer_ty = Box::pin(resolve_annotation(
                outer,
                span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            ))
            .await?;
            Ok(make_typevalue_app(outer_ty, arg))
        }
    }
}

/// Resolve an annotation in a context where a type expression is expected.
/// Unlike `resolve_annotation`, a PropertyDict is interpreted as a type expression
/// (record type or function type) rather than a property bag.
async fn resolve_annotation_as_type(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    match ann {
        Annotation::Quote => {
            // The quoting annotation constrains the value to be an AST node — a
            // runtime-determined type. The type checker cannot statically determine
            // which concrete type is produced, so produce a fresh type variable
            // (consistent with typecheck_narrow.rs:368).
            Ok(state.fresh_type_var(&span))
        }
        Annotation::Expr(node) => match &node.expr {
            SurfaceExpression::VarRef {
                name,
                annotation: None,
                ..
            } => {
                let row_ref: Option<&HashMap<String, String>> =
                    row_ann_mapping.as_ref().map(|m| &**m);
                resolve_type_name(
                    name,
                    span,
                    state,
                    constraints,
                    ann_mapping,
                    type_params_scope,
                    &row_ref,
                )
                .await
            }
            SurfaceExpression::Dict { entries: surface_entries, .. } => {
                // Check for the @[type: T] shorthand — user-written annotations like
                // `x@[type: Int  default: 0]` where "type:" specifies the type alongside metadata.
                // When all keys are annotation metadata keys and "type:" is present, resolve
                // the type: value as the type expression rather than as a structural record.
                //
                // This mirrors the same shorthand detection in resolve_annotation (line ~1599)
                // so that `Fn@SomeType [params]` resolves the Fn's return type as TyCon("SomeType")
                // rather than Dict({type: TyCon("SomeType")}).
                //
                // Metadata keys: "type", "default", "repr", "doc", "is"
                // Non-metadata key (e.g. "id", "name") means @[type: T  id: X] is a structural
                // record — the "type" here is just a field name, not the shorthand.
                const METADATA_KEYS: &[&str] = &["type", "default", "repr", "doc", "is"];
                let has_non_metadata_key = surface_entries.iter().any(|se| {
                    if let Some(ref k) = se.node.key {
                        match &k.expr {
                            SurfaceExpression::StringLiteral { content: s, .. } => {
                                !METADATA_KEYS.contains(&s.as_str())
                            }
                            _ => true, // non-string key → treat as non-metadata
                        }
                    } else {
                        true // positional entry → not the @[type: T] shorthand
                    }
                });
                if !has_non_metadata_key {
                    if let Some(type_node) = surface_entries.iter().find_map(|se| {
                        let key_node = se.node.key.as_ref()?;
                        match &key_node.expr {
                            SurfaceExpression::StringLiteral { content: s, .. } if s == "type" => {
                                Some(&se.node.value)
                            }
                            _ => None,
                        }
                    }) {
                        return Box::pin(resolve_type_expr(
                            type_node,
                            state,
                            constraints,
                            ann_mapping,
                            row_ann_mapping,
                            type_params_scope,
                        ))
                        .await;
                    }
                }
                // All-positional or has non-metadata keys: structural type, function type, or
                // type constructor application. Delegate to resolve_type_dict.
                resolve_type_dict(
                    surface_entries,
                    span,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    TypeDictCtx {
                        type_params_scope,
                        tycon_name: "",
                    },
                )
                .await
            }
            _ => match ann.as_application() {
                // Chained type application (formerly Annotated(outer, inner)): for fn
                // annotations, forward to the full resolver (e.g. fn@Seq@Int should
                // resolve the application properly).
                Some((outer, inner)) => {
                    resolve_application(
                        &outer,
                        &inner,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await
                }
                // Any other SurfaceExpression shape: fall back to the general
                // type-expression resolver, which already handles the full range of
                // raw-node type syntax (and correctly errors on anything invalid).
                None => {
                    Box::pin(resolve_type_expr(
                        node,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await
                }
            },
        },
    }
}

pub(crate) async fn resolve_annotation(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    match ann {
        Annotation::Quote => {
            // The quoting annotation constrains the value to be an AST node — a
            // runtime-determined type. The type checker cannot statically determine
            // which concrete type is produced, so produce a fresh type variable
            // (consistent with typecheck_narrow.rs:368).
            Ok(state.fresh_type_var(&span))
        }
        Annotation::Expr(node) => match &node.expr {
            SurfaceExpression::VarRef {
                name,
                annotation: None,
                ..
            } => {
                let row_ref: Option<&HashMap<String, String>> =
                    row_ann_mapping.as_ref().map(|m| &**m);
                resolve_type_name(
                    name,
                    span,
                    state,
                    constraints,
                    ann_mapping,
                    type_params_scope,
                    &row_ref,
                )
                .await
            }
            SurfaceExpression::Dict { entries: surface_entries, .. } => {
                // A record-type-literal-shaped annotation can mean different things
                // depending on its keys:
                //
                // 1. Type shorthand: @[type: T  default: V  repr: R]
                //    If a "type:" key is present, resolve its value as the annotation type.
                //    The "default:" and "repr:" keys are handled separately by resolve_type_assert.
                //
                // 2. Structural type: @[field: Type ...], @[or A B], @[Seq Int], etc.
                //    No "type:" key → delegate to resolve_property_dict_as_record which calls
                //    resolve_type_dict (handles union/intersection/record types, type constructors,
                //    Fn type expressions) and falls back to Unknown for metadata-only annotations.
                //
                // Check for the "type:" key using SurfaceEntry directly (avoids allocation when
                // the key is present and we only need the value node).
                //
                // The "type:" shorthand ONLY applies when all keys are annotation metadata
                // (type:, default:, repr:, doc:, is:). If there are non-metadata keys like "id:", "name:",
                // etc., the annotation must be a structural record type — "type:" is a field name.
                // E.g.: @[type: String] → Type::Str (shorthand)
                //       @[type: String id: Int] → Record{type: Str, id: Int} (structural)
                // "is:" is narrowing metadata: @[is: Int] declares a type predicate narrowing hint
                // for path-sensitive type narrowing in if/match conditions.
                let metadata_keys = ["type", "default", "repr", "doc", "is"];
                let has_non_metadata_key = surface_entries.iter().any(|se| {
                    if let Some(ref k) = se.node.key {
                        match &k.expr {
                            SurfaceExpression::StringLiteral { content: s, .. } => {
                                !metadata_keys.contains(&s.as_str())
                            }
                            _ => true, // non-string key → non-metadata
                        }
                    } else {
                        true // positional entry → non-metadata
                    }
                });

                let type_value_node = if !has_non_metadata_key {
                    surface_entries.iter().find_map(|se| {
                        let key_node = se.node.key.as_ref()?;
                        match &key_node.expr {
                            SurfaceExpression::StringLiteral { content: s, .. } if s == "type" => {
                                Some(&se.node.value)
                            }
                            _ => None,
                        }
                    })
                } else {
                    None
                };

                // When all keys are annotation metadata but no "type:" key is present,
                // the annotation carries only metadata (e.g., @[is: Int], @[default: 3],
                // @[doc: "..."]). Return Unknown: the metadata doesn't define a type.
                // Note: "label" is not in metadata_keys, so @[label: l] takes the normal
                // has_non_metadata_key path and is handled by the label: branch below.
                if !has_non_metadata_key && type_value_node.is_none() {
                    return Ok(make_typevalue_unknown());
                }

                if let Some(type_node) = type_value_node {
                    // @[type: T ...] shorthand — resolve the type: value as a type expression.
                    resolve_type_expr(
                        type_node,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await
                } else if let Some(label_value_node) = surface_entries.iter().find_map(|se| {
                    // @[label: name] — named Label-kinded TypeVar annotation.
                    // Only fires when there is exactly one entry and its key is "label".
                    if surface_entries.len() == 1 {
                        let key_node = se.node.key.as_ref()?;
                        match &key_node.expr {
                            SurfaceExpression::StringLiteral { content: s, .. } if s == "label" => {
                                Some(&se.node.value)
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                }) {
                    // @[label: name] — validate and create a Label-kinded TypeVar.
                    //
                    // Validation rules (from doc/07-type-extensions.md §Label polymorphism):
                    // 1. Value must be a bare identifier (VarRef), not a string literal.
                    // 2. The identifier must start with a lowercase letter (label TypeVars are
                    //    lowercase by convention, mirroring type-kind TypeVars).
                    match &label_value_node.expr {
                        SurfaceExpression::StringLiteral { .. } => Err(Diagnostic::error(
                            KIND,
                            "label: value must be a bare name (e.g. `label: l`), not a string literal",
                            span,
                        )),
                        SurfaceExpression::VarRef { name, .. } => {
                            if name.starts_with(|c: char| c.is_uppercase()) {
                                Err(Diagnostic::error(KIND,
                                    format!(
                                        "label: value must be a lowercase type variable name (e.g. `label: l`), got '{}'",
                                        name
                                    ),
                                    span,
                                ))
                            } else {
                                // Valid lowercase label name: create a Label-kinded TypeVar.
                                // If we're inside a function scope (ann_mapping is Some), reuse the
                                // same TypeVar for the same label name across multiple params
                                // (same-name label vars must share the same TypeVar).
                                let fresh = if let Some(ref mut mapping) = ann_mapping {
                                    if let Some(existing_var) = mapping.get(name.as_str()) {
                                        existing_var.clone()
                                    } else {
                                        let level = state.ctx.current_level;
                                        let (v, _) = state.fresh_type_var_with(
                                            Some("_label"),
                                            Some(level),
                                            "Label",
                                            &label_value_node.span,
                                        );
                                        // kind_env removed in S-1003 — Label-kinded TypeVars are tracked via ctx.levels.
                                        state.type_var_source_names.insert(v.clone(), name.clone());
                                        mapping.insert(name.clone(), v.clone());
                                        v
                                    }
                                } else {
                                    let level = state.ctx.current_level;
                                    let (v, _) = state.fresh_type_var_with(
                                        Some("_label"),
                                        Some(level),
                                        "Label",
                                        &label_value_node.span,
                                    );
                                    // kind_env removed in S-1003
                                    v
                                };
                                assert!(
                                    state.get_level(&fresh).is_some(),
                                    "invariant: label var just inserted must be present in state.ctx.levels"
                                );
                                Ok(make_typevalue_var(&fresh))
                            }
                        }
                        _ => Err(Diagnostic::error(
                            KIND,
                            "label: value must be a bare name (e.g. `label: l`)",
                            span,
                        )),
                    }
                } else {
                    // Validate: all keyed entries must have string (bare-word) keys.
                    // Integer keys (e.g., @[42: Int]) are not valid type annotation syntax.
                    for se in surface_entries.iter() {
                        if let Some(ref k) = se.node.key {
                            match &k.expr {
                                SurfaceExpression::StringLiteral { .. }
                                | SurfaceExpression::VarRef { .. } => {}
                                _ => {
                                    return Err(Diagnostic::error(
                                        "invalid-annotation",
                                        "property dict type annotation requires string (bare-word) keys",
                                        k.span.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    // No "type:" key (or has non-metadata keys) — treat as structural type or metadata.
                    Box::pin(resolve_property_dict_as_record(
                        surface_entries,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await
                }
            }
            _ => match ann.as_application() {
                // @Outer@Inner means: apply type constructor `outer` to type argument
                // `inner` (formerly the separate Annotated variant) — shared handling.
                Some((outer, inner)) => {
                    resolve_application(
                        &outer,
                        &inner,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await
                }
                // Any other SurfaceExpression shape: fall back to the general
                // type-expression resolver.
                None => {
                    Box::pin(resolve_type_expr(
                        node,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await
                }
            },
        },
    }
}

async fn resolve_property_dict_as_record(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    let dict_result = resolve_type_dict(
        entries,
        span.clone(),
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        TypeDictCtx {
            type_params_scope,
            tycon_name: "",
        },
    )
    .await;

    match dict_result {
        Ok(ty) => Ok(ty),
        Err(e) => {
            let is_tycon_error = entries.first().is_some_and(|first| {
                first.node.key.is_none()
                    && matches!(&first.node.value.expr, SurfaceExpression::VarRef { name, .. }
                        if state.type_name_to_id.get(name.as_str()).is_some_and(|id| state.tycon_env.contains_key(id)))
            });
            if entries_look_like_type_dict(entries) || is_tycon_error {
                Err(e)
            } else {
                // The property dict is not a type-dict (no recognized type head, no matching
                // record-field shape). Try evaluating it as a type-stage expression —
                // type-stage combinators like `@[my-combinator args]` are handled by
                // eval_type_stage_expr.
                //
                // Synthesize an Arc<SurfaceNode> from the PropertyDict entries so
                // eval_type_stage_expr can evaluate it in the type-stage environment.
                //
                // Annotation PropertyDicts with all-positional entries whose first entry is
                // a VarRef represent implied calls: @[my-combinator Int String] is parsed as
                // PropertyDict([{key:None, val:VarRef("my-combinator")}, ...]) rather than as
                // a Dict expression. We must detect this case and synthesize a Call node so
                // the evaluator sees a function call, not an integer-keyed dict.
                //
                // `e` is the probe error from resolve_type_dict — it is not a real error when
                // eval_type_stage_expr succeeds (the dict was a type-stage expression, not a
                // type dict). If eval_type_stage_expr also fails, propagate `e` as the original
                // cause rather than silencing it.
                let synth_node = synthesize_type_stage_node(entries, span.clone());
                let (etse_result, _etse_diags) = eval_type_stage_expr(&synth_node, state).await.into_parts();
                match etse_result {
                    Ok(ty) => Ok(ty),
                    Err(eval_err) => {
                        // Both resolve_type_dict and eval_type_stage_expr failed.
                        // Return eval_err as primary — it reflects the most recent and
                        // most specific resolution attempt (the type-stage expression
                        // path). Attach the type-dict probe error `e` as a note so
                        // both failures are visible to the user.
                        Err(eval_err.with_note(format!("also tried as type dict: {}", e.message)))
                    }
                }
            }
        }
    }
}

/// Check whether property dict entries structurally look like they could be a
/// type dict (record type or function type expression). Returns true when all
/// entries look like record-type fields (string key + type-expression value),
/// or when the first entry matches the `Fn@Return [Params]` function type
/// pattern. When entries contain literal values (Int, Float, Bool) or
/// auto-indexed non-function entries, they are annotation metadata rather than
/// type definitions, and type resolution errors should be swallowed.
fn entries_look_like_type_dict(entries: &[Spanned<SurfaceEntry>]) -> bool {
    // Detect `[Fn@Return [Params]]` function type pattern: first entry is
    // auto-indexed with an Annotated node whose name is "Fn".
    if let Some(first) = entries.first() {
        if first.node.key.is_none() {
            // Annotated VarRef (annotation is now on VarRef directly).
            if let SurfaceExpression::VarRef {
                name,
                annotation: Some(_),
                ..
            } = &first.node.value.expr
            {
                if name == "Fn" {
                    return true;
                }
            }
        }
    }

    // Record type pattern: every entry has a string key and a type-shaped value.
    entries.iter().all(|entry| {
        // Rest entries (`...` / `...name`) are valid in type dicts
        if matches!(&entry.node.value.expr, SurfaceExpression::Placeholder(..)) {
            return true;
        }
        // Every entry must have a string key
        let has_str_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| matches!(&k.expr, SurfaceExpression::StringLiteral { .. }));
        // Value must be a form that could be a type expression
        let value_is_type_shaped = matches!(
            &entry.node.value.expr,
            SurfaceExpression::StringLiteral { .. }
                | SurfaceExpression::VarRef { .. }  // includes annotated VarRef
                | SurfaceExpression::Dict { .. }
        );
        has_str_key && value_is_type_shaped
    })
}

/// Extract the class name string from a ConstraintDecl TypeValue.
///
/// ConstraintDecl payload: `{ class: TypeValue.Op { name: String }, args: Dict }`.
/// Returns `Some(class_name)` when the payload is fully settled, `None` otherwise.
fn extract_constraint_class_name_tv(cv: &TypeValue) -> Option<String> {
    let payload_thunk = match cv.as_ref() {
        Value::Variant {
            type_decl_id,
            ctor_index,
            payload: Some(t),
            ..
        } if *type_decl_id == crate::value::TYPE_ID_CONSTRAINT_DECL
            && *ctor_index == CD_CONSTRAINT_DECL =>
        {
            t
        }
        _ => return None,
    };
    let dict = match payload_thunk.peek_result()? {
        Ok(Value::Dict { entries, .. }) => entries,
        _ => return None,
    };
    let class_thunk = dict.get(&HashableValue::Str(Arc::from(FIELD_CLASS)))?;
    match class_thunk.peek_result()? {
        Ok(Value::Variant {
            ctor_index: c_idx,
            payload: Some(inner_thunk),
            ..
        }) if *c_idx == TV_NOMINAL => match inner_thunk.peek_result()? {
            Ok(Value::Dict { entries: inner, .. }) => {
                let name_thunk = inner.get(&HashableValue::Str(Arc::from(FIELD_NAME)))?;
                match name_thunk.peek_result()? {
                    Ok(Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the arg TypeValues from a ConstraintDecl TypeValue.
///
/// ConstraintDecl payload: `{ class: ..., args: { 0: TypeValue, 1: TypeValue, ... } }`.
/// Returns the args in integer-key order. Returns an empty Vec when the payload is unsettled
/// or the args dict is absent.
fn extract_constraint_args_tv(cv: &TypeValue) -> Vec<TypeValue> {
    let payload_thunk = match cv.as_ref() {
        Value::Variant {
            type_decl_id,
            ctor_index,
            payload: Some(t),
            ..
        } if *type_decl_id == crate::value::TYPE_ID_CONSTRAINT_DECL
            && *ctor_index == CD_CONSTRAINT_DECL =>
        {
            t
        }
        _ => return Vec::new(),
    };
    let dict = match payload_thunk.peek_result() {
        Some(Ok(Value::Dict { entries, .. })) => entries,
        _ => return Vec::new(),
    };
    let args_thunk = match dict.get(&HashableValue::Str(Arc::from(FIELD_ARGS))) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let args_entries = match args_thunk.peek_result() {
        Some(Ok(Value::Dict { entries, .. })) => entries,
        _ => return Vec::new(),
    };
    let mut result = Vec::with_capacity(args_entries.len());
    let mut i = 0i64;
    loop {
        match args_entries.get(&HashableValue::Int(i)) {
            Some(arg_thunk) => match arg_thunk.peek_result() {
                Some(Ok(v)) => result.push(Arc::new(v.clone())),
                _ => break,
            },
            None => break,
        }
        i += 1;
    }
    result
}

/// Instantiate a parameterized type alias by substituting type arguments for parameters.
///
/// Given `Pair: [type [a] [first: a second: a]]` and args `[Int]`,
/// builds substitution `{a -> Int}` and applies to body to get `[first: Int second: Int]`.
async fn instantiate_tycon_def(
    alias: &TyConDef,
    type_args: &[TypeValue],
    state: &mut InferState,
    span: Span,
) -> Result<TypeValue, Diagnostic> {
    // Build substitution from parameter names to provided types
    let mut type_subst: HashMap<String, TypeValue> = HashMap::new();
    for (param, arg) in alias.params.iter().zip(type_args.iter()) {
        type_subst.insert(param.clone(), arg.clone());
    }

    // Check constraints — each constraint on a type param must have a satisfying instance for
    // the concrete type argument provided. For each ConstraintDecl in alias.constraints:
    //   1. Apply the substitution to get the concrete class and arg types.
    //   2. Extract the class name and the concrete arg TypeValues.
    //   3. Map each arg to a dispatch tag via type_to_dispatch_tag.
    //   4. If all args have concrete tags, build the mangled instance binding name and check
    //      that an instance exists. If no instance is found, emit a Diagnostic::error.
    //
    // When alias.constraints is empty (the common case — TyConDef.constraints is currently
    // always Vec::new()), this loop is a no-op. The check is live for any future type alias
    // that carries constraint annotations on its type params.
    for raw_constraint in &alias.constraints {
        // Apply the type-arg substitution to the constraint so TypeVar refs become concrete.
        let concrete_constraint =
            crate::types::apply_typevalue_renaming(raw_constraint, &type_subst);

        // Extract the class name from the substituted ConstraintDecl.
        let class_name = match extract_constraint_class_name_tv(&concrete_constraint) {
            Some(n) => n,
            None => continue, // Non-ConstraintDecl or unsettled thunk — skip.
        };

        // Extract concrete arg TypeValues from the substituted constraint's args dict.
        let concrete_args = extract_constraint_args_tv(&concrete_constraint);

        // Map each concrete arg to a dispatch tag. If any arg is not a concrete type
        // (e.g., still a TypeVar), skip constraint checking for this constraint — it
        // will be resolved later by the general constraint-discharge mechanism.
        let dispatch_tags: Option<Vec<String>> = concrete_args
            .iter()
            .map(|tv| crate::typecheck::type_to_dispatch_tag(tv))
            .collect();

        let tags = match dispatch_tags {
            Some(t) => t,
            None => continue, // Polymorphic arg — constraint not checkable at instantiation.
        };

        // Check that an instance exists for this class + concrete arg types.
        // Instance keys are mangled per method: ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽.
        // To check class-level satisfaction, look up any registered method for this class
        // from ClassDecl.method_signatures and check that a mangled instance key exists for
        // at least one method with the given type tags.
        let instance_found = {
            let class_opt = state.classes.iter().find(|c| c.name == class_name);
            match class_opt {
                None => true, // Unknown class — not yet in env (e.g., forward reference); defer checking.
                Some(class_decl) => {
                    if class_decl.method_signatures.is_empty() {
                        // No method signatures registered yet (class body not yet processed).
                        // Defer constraint checking — assume satisfied to avoid false errors.
                        true
                    } else {
                        let _tags_ref: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
                        class_decl
                            .method_signatures
                            .iter()
                            .any(|(method_name, _)| state.instances.contains_key(method_name))
                    }
                }
            }
        };
        if !instance_found {
            return Err(Diagnostic::error(
                KIND,
                format!(
                    "type argument(s) {:?} do not satisfy constraint '{}' required by type alias",
                    tags, class_name,
                ),
                span.clone(),
            ));
        }
    }

    // Apply substitution to the alias body.
    // alias.body is TypeValue after S-1003 migration.
    Ok(crate::types::apply_typevalue_renaming(
        &alias.body,
        &type_subst,
    ))
}

/// Unified type-head resolution: given a name and optional already-resolved type arguments,
/// produce a TypeValue. This is the single canonical lookup path for all forms:
///   - Bare name: `@Comparable`, `@Integer`, `@Seq` — call with `args = &[]`
///   - Type application: `[Seq a]`, `[Iterable a]`, `[Map K V]` — call with resolved args
///
/// Lookup order:
///   1. `state.type_stage_data.scope` (resolved TypeValues): return directly (with App wrapping if args present)
///   2. `state.type_stage_data.fns` (Function thunks): call the thunk with args
///   3. `state.type_stage_data.type_vars` (TypeVar kinds): fresh TypeVar of the given kind
///   4. `state.env` (ClassDecl): fresh TypeVar with a class constraint
///   5. Undefined → Diagnostic
///
/// The lowercase path (ann_mapping, type_params_scope, cross-kind collision) is handled
/// by `resolve_type_name` before calling this function — `resolve_type_head` only handles
/// the case where we have a name that refers to a type constructor, class, or kind-annotated var.
async fn resolve_type_head(
    name: &str,
    args: &[TypeValue],
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<TypeValue, Diagnostic> {
    // Step 0: look up name in the unified scope chain (accumulated from all documents).
    // Type expressions must resolve type-stage bindings before runtime-stage ones, the
    // same stage priority `resolve.rs::resolve_name` uses for type-expression VarRefs —
    // a name may legitimately be bound at both stages (e.g. a type-stage alias to a
    // TypeNode, and an unrelated runtime-stage binding of the same name), and stage
    // priority must override mere lexical depth. `get_in_chain_staged` implements this;
    // it must NOT be replaced with a flat/depth-only scan.
    let mut scope_lookup_info: Option<String> = None;
    let _ = &scope_lookup_info;
    let (chain_result, chain_diags) = crate::symbol_table::SymbolTable::get_in_chain_staged(
        &state.current_scope,
        name,
        &[
            crate::symbol_table::Stage::Type,
            crate::symbol_table::Stage::Runtime,
        ],
    )
    .into_parts();
    state.diagnostics.extend(chain_diags);
    if let Some(thunk) = chain_result? {
        if let Some(eval_ctx) = &state.eval_ctx {
            let (mat_result, _mat_diags) = crate::eval::materialize(&thunk, None, eval_ctx)
                .await
                .into_parts();
            let val = mat_result?;
            let (tv_result, _tv_diags) = typenode_value_to_type(
                &val,
                eval_ctx,
                &state.type_stage_data.scope,
                &state.type_name_to_id,
            )
            .await
            .into_parts();
            let convert_result = tv_result?;
            if let Some(tv) = convert_result {
                if args.is_empty() {
                    return Ok(tv);
                }
                let mut result = tv;
                for arg in args {
                    result = make_typevalue_app(result, Arc::clone(arg));
                }
                return Ok(result);
            }
            scope_lookup_info = Some(format!(
                "found in scope, materialized to type_decl_id={}, but typenode_value_to_type returned None",
                val.type_decl_id_of(),
            ));
        } else {
            scope_lookup_info = Some("found in scope but no eval_ctx available".to_string());
        }
    }
    // If not found in scope, build diagnostic info.
    if scope_lookup_info.is_none() {
        let cs_count = state.current_scope.len();
        scope_lookup_info = Some(format!(
            "not found in scope (current: {} frames)",
            cs_count
        ));
    }

    // Check type_stage_type_vars for a TypeVar kind annotation.
    if let Some(kind) = state.type_stage_data.type_vars.get(name).cloned() {
        // Operator-kinded names in application position produce App chains.
        // This handles `[m SomeType]` where `m@Operator` was declared in a class.
        let kind_is_operator = kind == "Operator";
        if kind_is_operator && !args.is_empty() {
            if args.len() != 1 {
                return Err(Diagnostic::error(
                    KIND,
                    format!(
                        "type constructor `{name}` requires 1 type argument, got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let a_type = Arc::clone(&args[0]);
            if crate::type_tags::typevalue_ctor_index(&a_type) == Some(TV_NOMINAL) {
                let op_id = crate::type_infer::typevalue_nominal_id(&a_type)
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                return Err(Diagnostic::error(
                    KIND,
                    format!(
                        "kind mismatch: type constructor `{name}` cannot be \
                         applied to another type constructor `{op_id}`; \
                         use a concrete type instead"
                    ),
                    span,
                ));
            }
            return Ok(make_typevalue_app(
                make_typevalue_nominal(*state.type_name_to_id.get(name).unwrap_or(&0)),
                a_type,
            ));
        }
        let level = state.ctx.current_level;
        let (fresh, _fresh_ty) =
            state.fresh_type_var_with(Some(name), Some(level), kind.as_str(), &span);
        return Ok(make_typevalue_var(&fresh));
    }

    // Step 4: check env (class registry) for a class declaration.
    let class_decl_opt = state.classes.iter().find(|c| c.name == name).cloned();
    if let Some(class_decl) = class_decl_opt {
        let level = state.ctx.current_level;
        let (fresh, _fresh_ty) = state.fresh_type_var_with(Some(name), Some(level), "Type", &span);
        // Build a ConstraintDecl TypeValue using the new Arc<Value> API (T-1988 migration).
        // The class is represented as TypeValue.Op{name: class_name}.
        let class_op = crate::type_class::make_type_op(&class_decl.name);
        let mut constraint_args = vec![make_typevalue_var(&fresh)];
        constraint_args.extend(args.iter().cloned());
        let constraint_tv = crate::type_class::make_constraint_decl(class_op, constraint_args);
        // After S-1003: constraints is Vec<Arc<Value>>, push directly.
        constraints.push(constraint_tv);
        return Ok(make_typevalue_var(&fresh));
    }

    // Step 5: check tycon_env for type declarations (e.g., Boolean: [type True False]) and
    // structural type aliases (e.g. CodecInvalidHandler: [type [bind: [i o]] [Fn ...]]).
    // Annotations resolve in the type-stage env first, then fall back to runtime scope.
    // This handles types declared in the same dict or an enclosing runtime scope.
    //
    // This is the single canonical mechanism for resolving `TyCon` / `TyCon Arg1 Arg2 ...`
    // for BOTH syntactic forms (`Seq@Int` chained-annotation via resolve_application, and
    // `[Seq Int]` implied-call via resolve_type_expr) — callers must resolve their own
    // argument TypeValues and delegate here rather than re-deriving this logic.
    //
    // `def.constructors.is_empty()` is the exact, already-computed discriminant between a
    // nominal ADT (non-empty: real NominalVariant tags were extracted by
    // `build_tycon_def_for_entry` — never substitute into `body`, which would discard the
    // type's nominal identity, swap its TypeValue ctor tag away from Nominal/App, and break
    // every downstream nominal-identity check) and a structural type alias (empty — `body`
    // IS the type definition; substituting type args into it and returning the result is
    // the only correct way to resolve `[AliasName Arg1 Arg2]`, since no lazy/on-demand
    // alias-unfolding exists anywhere else in the type checker — `bas.rs`'s subtyping never
    // substitutes into `tycon_env[id].body`).
    if let Some(def) = state
        .type_name_to_id
        .get(name)
        .and_then(|id| state.tycon_env.get(id))
        .cloned()
    {
        let type_decl_id = *state.type_name_to_id.get(name).unwrap_or(&0);
        if def.constructors.is_empty() {
            // Structural type alias: always require exact arity. Unlike a nominal ADT, a
            // partially-applied alias has no sensible substituted body to fall back to —
            // there is no separate "generic head" identity for an alias the way `Nominal(id)`
            // serves that role for an ADT (used by Operator-kinded partial references).
            if args.len() != def.params.len() {
                return Err(Diagnostic::error(
                    KIND,
                    format!(
                        "type alias '{name}' expects {} type parameter(s), got {}",
                        def.params.len(),
                        args.len()
                    ),
                    span,
                ));
            }
            return Box::pin(instantiate_tycon_def(def.as_ref(), args, state, span)).await;
        }
        // Nominal ADT: wrap in App, preserving nominal identity (never substitute into
        // `def.body`). Partial application (fewer args than arity) is legitimate — it
        // produces a type-constructor-shaped App chain used e.g. for Operator-kinded
        // references. Providing MORE args than the type's arity is always a user error,
        // never something to silently truncate.
        if args.is_empty() {
            return Ok(make_typevalue_nominal(type_decl_id));
        }
        if args.len() > def.arity() {
            return Err(Diagnostic::error(
                KIND,
                format!(
                    "type constructor '{name}' takes at most {} type argument(s), got {}",
                    def.arity(),
                    args.len()
                ),
                span,
            ));
        }
        let mut result = make_typevalue_nominal(type_decl_id);
        for arg in args {
            result = make_typevalue_app(result, Arc::clone(arg));
        }
        return Ok(result);
    }

    // Not found in any scope — undefined type name.
    let mut diag = Diagnostic::error(
        "type-check/stage:type",
        format!("undefined type: {name}"),
        span.clone(),
    );
    diag.labels = vec![crate::error::SpanLabel {
        span,
        message: format!("type `{name}` not found in scope"),
    }];
    if let Some(info) = scope_lookup_info {
        diag = diag.with_note(info);
    }
    for (i, frame) in state.current_scope.iter().enumerate() {
        let entry_names: Vec<&str> =
            frame.entries().keys().map(|s| s.as_str()).take(8).collect();
        let suffix = if frame.len() > 8 {
            format!(" ... ({} total)", frame.len())
        } else {
            String::new()
        };
        diag = diag.with_note(format!(
            "current_scope[{}] ({:?}): [{}{}]",
            i,
            frame.stage(),
            entry_names.join(", "),
            suffix,
        ));
    }
    Err(diag)
}

pub(crate) async fn resolve_type_name(
    name: &str,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
    row_ann_mapping: &Option<&HashMap<String, String>>,
) -> Result<TypeValue, Diagnostic> {
    match name {
        // All fundamental types (Integer, String, Float, Bytes, Never, Any,
        // Proxy, Dict, Expr, Unknown, Operator, Label) are declared in builtin_core.llt
        // and resolved through the scope chain in resolve_type_head.
        // `@Unknown` resolves via type_stage_scope populated from the evaluated
        // builtin_core.llt type-stage section (both production and test/LSP paths).
        // `@Operator` and `@Label` resolve via type_stage_type_vars (kind annotations).
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
                // Type parameter scope enforcement (T-1100 / T-951).
                // When inside a TypeAlias body resolution (type_params_scope is Some),
                // lowercase names are TypeVars ONLY if they appear in the declared params list.
                // Unknown lowercase names are a type error rather than silently creating a
                // fresh TypeVar — enforcing the "explicit type params" principle.
                if let Some((params, strict)) = type_params_scope {
                    // If this name is a bound type parameter in scope, return its TypeVar
                    // directly. Class/instance/alias params introduced via [let ...] live here.
                    if let Some(tv) = params.get(name) {
                        return Ok(tv.clone());
                    }
                    // Strict mode (TypeAlias bodies): validate that undeclared lowercase
                    // names are either scope references or error. Non-strict (class methods):
                    // allow names to fall through to the ann_mapping lookup below, which will
                    // either find a bind:-declared TypeVar or produce a Diagnostic.
                    let in_params = ann_mapping.as_ref().is_some_and(|m| m.contains_key(name));
                    if strict && !in_params && !params.contains_key(name) {
                        // Name not declared as a type parameter — check if it's a scope reference.
                        if !state
                            .type_name_to_id
                            .get(name)
                            .is_some_and(|id| state.tycon_env.contains_key(id))
                        {
                            return Err(Diagnostic::error(
                                KIND,
                                format!(
                                    "undefined name '{name}' in type alias body — \
                                     lowercase names must be declared as type parameters \
                                     with [let ...] or must refer to a type in scope"
                                ),
                                span,
                            ));
                        }
                        // It's a scope reference — fall through to normal resolution.
                    }
                }

                // Cross-kind collision check (row→type direction): if the name was already
                // registered as a row variable (in row_ann_mapping), it cannot also be used
                // as a type variable. This is the symmetric counterpart to the type→row check
                // in resolve_type_dict (which checks ann_mapping before registering in
                // row_ann_mapping).
                // Cross-kind collision: a name used as row variable cannot also
                // be used as a type variable — UNLESS it was pre-seeded in both maps
                // (parameterized type alias params can appear in either position).
                let in_row = row_ann_mapping
                    .as_ref()
                    .is_some_and(|m| m.contains_key(name));
                let in_ann = ann_mapping.as_ref().is_some_and(|m| m.contains_key(name));
                if in_row && !in_ann {
                    return Err(Diagnostic::error(KIND,
                        format!(
                            "annotation name '{name}' is already used as a row variable in this function; \
                             it cannot also be used as a type variable"
                        ),
                        span,
                    ));
                }

                // If we have an annotation mapping (within a function), check if this
                // annotation name has already been mapped to a fresh variable.
                // TypeVars must be explicitly declared via bind: — implicit creation is
                // not allowed. If the name is not in the mapping, it is a type error.
                if let Some(ref mut mapping) = ann_mapping {
                    // Check if this annotation name already has a mapping
                    if let Some(existing_var) = mapping.get(name) {
                        // Already mapped: return the existing TypeVar with its current level
                        // from state.ctx.levels. DO NOT reset the level - unification may have
                        // lowered it, and level lowering must be monotone (Kiselyov 2013).
                        assert!(
                            state.get_level(existing_var).is_some(),
                            "invariant: annotation var registered in mapping must be in state.ctx.levels"
                        );
                        Ok(make_typevalue_var(existing_var))
                    } else {
                        Err(Diagnostic::error(
                            KIND,
                            format!("undefined type: {name}"),
                            span,
                        ))
                    }
                } else {
                    Err(Diagnostic::error(
                        KIND,
                        format!("undefined type: {name}"),
                        span,
                    ))
                }
            } else {
                // Uppercase type name — route through the unified resolve_type_head.
                // Lookup order: type_stage_scope, type_stage_fns, type_stage_type_vars, env classes.
                Box::pin(resolve_type_head(name, &[], state, constraints, span)).await
            }
        }
    }
}

pub(crate) async fn resolve_type_expr(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<TypeValue, Diagnostic> {
    match &node.expr {
        // Integer literals in type position → TypeValue.IntLit.
        // Enables `@[or 0 1]` and similar union-of-literals annotations in the static
        // annotation path (no type-stage evaluation needed).
        SurfaceExpression::Int(n) => Ok(make_typevalue_int_lit(*n)),
        // String literals in type position → TypeValue.StrLit (tag-only enum variants).
        // VarRef still goes to resolve_type_name for type alias lookup.
        SurfaceExpression::StringLiteral { content: s, .. } => Ok(make_typevalue_str_lit(s)),
        // Annotated VarRef (name@Type): annotation is now on VarRef directly.
        // Must come before the plain VarRef arm to be reachable.
        SurfaceExpression::VarRef {
            name,
            annotation: Some(annotation),
            ..
        } => {
            if name == "Fn" {
                Box::pin(resolve_fn_type(
                    &annotation.node,
                    annotation.span.clone(),
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                ))
                .await
            } else {
                // For all other parameterized type annotations in type-expression position
                // (e.g., `Handle@DirCap`, `Seq@Int`, `Map@[key: Str value: Int]` inline),
                // `node` already IS the VarRef-with-nested-annotation shape Annotation::Expr
                // wraps — no reconstruction needed (formerly rebuilt as a synthetic
                // Annotated(Simple(name), inner) before dispatching to resolve_annotation,
                // which handles `"Handle"`, `"Seq"`, `"Map"`, etc. via `as_application`).
                Box::pin(resolve_annotation(
                    &Annotation::Expr(Arc::clone(node)),
                    node.span.clone(),
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                ))
                .await
            }
        }
        SurfaceExpression::VarRef {
            name,
            annotation: None,
            ..
        } => {
            // Primitive type names must be resolved as type names, not nominal variant
            // constructors.  Int, Float, String, Bool, Number etc. all start with an uppercase
            // letter and therefore match `is_constructor_name`, but they are NOT variants —
            // they are built-in type names handled by `resolve_type_name`.
            //
            // Resolution order:
            //   1. If the name matches a known primitive or registered type alias via
            //      `resolve_type_name`, use that result (handles Int, Float, String, Bool,
            //      Number, top-level aliases, etc.).
            //   2. Otherwise (undefined type), if the name starts with uppercase, treat it
            //      as a unit nominal variant constructor — e.g. `None` in
            //      `[type [Option a] [Some a] None]`.
            //   3. For lowercase names, propagate the `resolve_type_name` error directly.
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            match resolve_type_name(
                name,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                type_params_scope,
                &row_ref,
            )
            .await
            {
                Ok(ty) => Ok(ty),
                Err(e) if crate::eval::is_constructor_name(name) => {
                    // Recover only for "type-check" errors on constructor-shaped names
                    // (uppercase initial character). These arise when a bare uppercase name
                    // like `None` or `Red` is used in annotation position before the type
                    // declaration is registered — the name is a unit nominal variant.
                    // Propagate all other errors (e.g., kind mismatches, internal errors).
                    if e.message.starts_with("undefined type:") {
                        Ok(make_typevalue_nominal_variant(
                            &lookup_tycon_for_ctor(state, name),
                            name,
                            make_typevalue_record(indexmap::IndexMap::new(), None),
                        ))
                    } else {
                        Err(e)
                    }
                }
                Err(e) => Err(e),
            }
        }
        SurfaceExpression::Dict { entries, scope: _ } => {
            Box::pin(resolve_type_dict(
                entries,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                TypeDictCtx {
                    type_params_scope,
                    tycon_name: "",
                },
            ))
            .await
        }
        // This arm handles `SurfaceExpression::Call { implied: true }`, which arises when a
        // bare identifier (no `@` annotation) appears in head position inside a type expression,
        // e.g. `[Fn [Int Int]]` (missing the required `@` before the return type).
        //
        // NOTE: The inner `if let SurfaceExpression::Annotated` guard is currently unreachable.
        // `Fn@RetType` in head position is routed to `SurfaceExpression::Dict` by the parser's
        // Priority 2b rule (Identifier + ImmediateAt → Dict), so the func of any
        // `implied: true` Call is always `SurfaceExpression::VarRef`, never
        // `SurfaceExpression::Annotated`. The guard never fires; all implied calls in type
        // context fall through to the `Err(...)` at the end of this arm.
        SurfaceExpression::Call {
            implied: true,
            func,
            args,
            named_args,
            ..
        } => {
            // Annotated VarRef (annotation is now on VarRef directly).
            if let SurfaceExpression::VarRef {
                name,
                annotation: Some(annotation),
                ..
            } = &func.expr
            {
                if name == "Fn" {
                    // Fn@RetType [Params] in new syntax: resolve return type from annotation,
                    // then resolve each arg as a parameter type. For zero params, args is empty.
                    let ret = Box::pin(resolve_annotation_as_type(
                        &annotation.node,
                        annotation.span.clone(),
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await?;
                    // args[0] should be the parameter list (a Dict or another implied Call)
                    let mut params = Vec::new();
                    if let Some(param_list) = args.first() {
                        match &param_list.expr {
                            SurfaceExpression::Dict {
                                entries: param_entries,
                                scope: _,
                            } => {
                                for entry in param_entries {
                                    // Extract parameter name from key if present
                                    let param_name = if let Some(ref key) = entry.node.key {
                                        match &key.expr {
                                            SurfaceExpression::VarRef { name, .. } => {
                                                Some(name.clone())
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content: s, ..
                                            } => Some(s.clone()),
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    };
                                    let param_ty = Box::pin(resolve_type_expr(
                                        &entry.node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?;
                                    params.push((param_name, param_ty, entry.span.clone()));
                                }
                            }
                            SurfaceExpression::Call {
                                implied: true,
                                func: inner_func,
                                args: inner_args,
                                ..
                            } => {
                                let param_ty = Box::pin(resolve_type_expr(
                                    inner_func,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                params.push((None, param_ty, inner_func.span.clone()));
                                for a in inner_args.iter() {
                                    let param_ty = Box::pin(resolve_type_expr(
                                        a,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?;
                                    params.push((None, param_ty, a.span.clone()));
                                }
                            }
                            _ => {
                                let param_ty = Box::pin(resolve_type_expr(
                                    param_list,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                params.push((None, param_ty, param_list.span.clone()));
                            }
                        }
                    }
                    if args.len() > 1 {
                        return Err(Diagnostic::error(KIND,
                            format!(
                                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                                1 + args.len()
                            ),
                            node.span.clone(),
                        ));
                    }
                    return Ok(make_typevalue_fn(params, ret));
                }
            }

            // Type constructor application in implied-call position: `[TyCon Arg1 Arg2 ...]`.
            // Covers both nominal ADTs (Seq, Map, Handle, user-declared ADTs) and
            // parameterized structural type aliases (e.g. CodecInvalidHandler) — resolve
            // all arguments, then delegate entirely to `resolve_type_head`'s Step 5, the one
            // canonical mechanism that discriminates ADT-vs-alias and does App-wrapping or
            // substitution accordingly (shared with `Seq@Int`-style chained annotations via
            // `resolve_application`, and with `resolve_type_dict`'s unified-type-head block).
            // Do not reintroduce a parallel inline TyConDef lookup/branch here.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if state
                    .type_name_to_id
                    .get(name.as_str())
                    .is_some_and(|id| state.tycon_env.contains_key(id))
                {
                    let mut resolved_args = Vec::with_capacity(args.len());
                    for arg_node in args {
                        resolved_args.push(
                            Box::pin(resolve_type_expr(
                                arg_node,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?,
                        );
                    }
                    return Box::pin(resolve_type_head(
                        name,
                        &resolved_args,
                        state,
                        constraints,
                        node.span.clone(),
                    ))
                    .await;
                }
            }

            // Nominal constructor: [ConstructorName field1: T1 field2: T2 ...]
            // Check if func is an uppercase VarRef (nominal constructor name).
            // Builtin type names (Int, Float, etc.) must NOT be treated as NominalVariant.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                let is_builtin = state
                    .type_name_to_id
                    .get(name.as_str())
                    .and_then(|id| state.tycon_env.get(id))
                    .is_some();
                if crate::eval::is_constructor_name(name) && !is_builtin {
                    // This is a nominal variant constructor with named fields and/or constants.
                    //
                    // The lookup-table syntax mixes two distinct entry kinds:
                    //   - Constants: `name: literal` → named_arg with literal value — stored in
                    //     TyConDef.constructor_constants, NOT in the NominalVariant type.
                    //   - Payload fields: `name: TypeExpr` → named_arg with non-literal value, OR
                    //     `name@TypeExpr` → annotated positional arg.
                    //
                    // Separation rule:
                    //   named_args whose value resolves as a literal (Int/Float/Str/U64) → constant
                    //   named_args whose value is not a literal → payload field (resolve as type)
                    //   args that are Annotated { name, annotation } → named payload field
                    //   args that are bare VarRef/Call (not Annotated) → old-style positional payload
                    //
                    // `is_literal` helper: true for Int, U64, Float, StringLiteral surface expressions.
                    let is_literal_expr = |expr: &SurfaceExpression| {
                        matches!(
                            expr,
                            SurfaceExpression::Int(_)
                                | SurfaceExpression::U64(_)
                                | SurfaceExpression::Float(_)
                                | SurfaceExpression::StringLiteral { .. }
                        )
                    };

                    // Collect payload fields from non-literal named_args.
                    let payload_named: Vec<_> = named_args
                        .iter()
                        .filter(|na| !is_literal_expr(&na.node.value.expr))
                        .collect();

                    // Collect payload fields from annotated positional args (data@String form).
                    // Annotated VarRef: annotation is now on VarRef directly.
                    let payload_annotated: Vec<_> = args
                        .iter()
                        .filter_map(|arg| {
                            if let SurfaceExpression::VarRef {
                                name,
                                annotation: Some(annotation),
                                ..
                            } = &arg.expr
                            {
                                Some((name.clone(), annotation.clone(), arg.span.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    let has_payload_named = !payload_named.is_empty();
                    let has_payload_annotated = !payload_annotated.is_empty();

                    if has_payload_named || has_payload_annotated {
                        // Mixed constants + payload fields, or payload-only.
                        // Build NominalVariant with only the payload fields (constants live in TyConDef).
                        let mut fields_map = indexmap::IndexMap::new();

                        // Named payload fields from non-literal named_args.
                        for named_arg in &payload_named {
                            let field_ty = Box::pin(resolve_type_expr(
                                &named_arg.node.value,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            fields_map.insert(named_arg.node.name.clone(), field_ty);
                        }

                        // Named payload fields from annotated positional args (data@String).
                        // Resolve each annotation as a type expression.
                        for (field_name, annotation, ann_span) in &payload_annotated {
                            let field_ty = Box::pin(resolve_annotation(
                                &annotation.node,
                                ann_span.clone(),
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            fields_map.insert(field_name.clone(), field_ty);
                        }

                        return Ok(make_typevalue_nominal_variant(
                            &lookup_tycon_for_ctor(state, name),
                            name,
                            make_typevalue_record(fields_map, None),
                        ));
                    } else {
                        return Ok(make_typevalue_nominal_variant(
                            &lookup_tycon_for_ctor(state, name),
                            name,
                            make_typevalue_record(indexmap::IndexMap::new(), None),
                        ));
                    }
                }
            }

            // Lowercase VarRef in implied-call head position with args.
            //
            // Pattern: [a T1 T2 ...] where `a` is lowercase and has arguments.
            //
            // Route through eval_type_stage_expr so that any type-stage function defined
            // in the type-stage scope (e.g. `or`, `all`, `without`, or user-defined
            // combinators) is resolved uniformly. The type-stage scope contains these as
            // type_stage_fns entries; evaluation dispatches them correctly.
            //
            // This handles prelude annotations like `[return: [a Null]]` in:
            //   cond: [fn@[return: [a Null] doc: "..."] ...]
            //   when: [fn@[return: [a Null] doc: "..."] ...]
            //   unless: [fn@[return: [a Null] doc: "..."] ...]
            //
            // In these annotations, `a` is a type variable and `Null` is the empty record.
            // The parser sees `[a Null]` as an implied call `Call(VarRef("a"), [VarRef("Null")])`
            // because `a` in head position without `:` or `@` is treated as a function name.
            if let SurfaceExpression::VarRef {
                name: func_name, ..
            } = &func.expr
            {
                if func_name.starts_with(|c: char| c.is_lowercase()) && !args.is_empty() {
                    // Type combinator protocol: `or`, `all`, `without` are defined by the type
                    // language itself — any compliant prelude must implement these semantics.
                    // They map directly to Union, Intersection, and Negation in the type lattice.
                    // Handled in Rust because eval_type_stage_expr requires a full resolver pass
                    // and an EvalContext, neither of which is available at annotation resolution time.
                    // See doc/16b-rust-tinct-protocol.md §Type Combinators.
                    match func_name.as_str() {
                        "or" | "union" => {
                            let mut members = Vec::with_capacity(args.len());
                            for arg in args {
                                members.push(
                                    Box::pin(resolve_type_expr(
                                        arg,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?,
                                );
                            }
                            return Ok(typevalue_normalize_union_ctx(members, Some(&state.ctx)));
                        }
                        "all" => {
                            let mut members = Vec::with_capacity(args.len());
                            for arg in args {
                                members.push(
                                    Box::pin(resolve_type_expr(
                                        arg,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?,
                                );
                            }
                            return Ok(typevalue_normalize_intersection(members));
                        }
                        "without" => {
                            if let Some(inner_node) = args.first() {
                                let inner = Box::pin(resolve_type_expr(
                                    inner_node,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                return Ok(make_typevalue_negation(inner));
                            }
                        }
                        _ => {}
                    }
                    // General case: try eval_type_stage_expr for other lowercase-headed calls.
                    let (etse_result, _etse_diags) = eval_type_stage_expr(node, state).await.into_parts();
                    return etse_result;
                }
            }

            Err(Diagnostic::error(
                KIND,
                format!("invalid type expression in annotation: {:?}", node.expr),
                node.span.clone(),
            ))
        }
        _ => Err(Diagnostic::error(
            KIND,
            format!("invalid type expression in annotation: {:?}", node.expr),
            node.span.clone(),
        )),
    }
}

/// Reverse-lookup: given a bare constructor name (e.g. `"Red"`), find the tycon name
/// (e.g. `"Color"`) by scanning `state.tycon_env` for any tycon whose `constructors` vec
/// contains an entry whose display name (e.g. `"Color.Red"`) has a bare part matching
/// `ctor_name` (the part after the `.`).
///
/// Returns the tycon name (e.g. `"Color"`) if found, or an empty string if not.
/// Used by `resolve_type_expr` to populate `NominalVariant.tycon` when only the ctor
/// name is available (e.g. bare uppercase VarRef fallback, implied-call constructor).
fn lookup_tycon_for_ctor(state: &InferState, ctor_name: &str) -> String {
    for (tycon_id, def) in &state.tycon_env {
        for (bare_ctor, _arity) in &def.constructors {
            if bare_ctor == ctor_name {
                return state
                    .type_display_names
                    .get(tycon_id)
                    .cloned()
                    .unwrap_or_else(|| tycon_id.to_string());
            }
        }
    }
    String::new()
}

/// Context for `resolve_type_dict` that groups the two parameters which are conceptually
/// related to the type-alias being resolved: the type-parameter scope and the name of the
/// enclosing type constructor (used when constructing TypeValue.NominalVariant values).
///
/// Grouping them eliminates the 8th argument and keeps the function within Clippy's limit.
pub(crate) struct TypeDictCtx<'a> {
    /// Type parameter scope for the enclosing alias or class body, if any.
    pub type_params_scope: Option<(&'a HashMap<String, TypeValue>, bool)>,
    /// Name of the enclosing type constructor. `""` when resolving standalone annotations.
    pub tycon_name: &'a str,
}

pub(crate) async fn resolve_type_dict(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    tdc: TypeDictCtx<'_>,
) -> Result<TypeValue, Diagnostic> {
    let type_params_scope = tdc.type_params_scope;
    let tycon_name = tdc.tycon_name;
    if let Some(fn_type) = Box::pin(try_resolve_fn_type_expr(
        entries,
        span.clone(),
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
    ))
    .await?
    {
        return Ok(fn_type);
    }

    // Type combinator protocol: `or`, `all`, `without` are TYPE LANGUAGE constructs
    // defined by the Rust protocol (doc/16b-rust-tinct-protocol.md §Type Combinators).
    // They map directly to Union, Intersection, and Negation and must be handled here
    // because eval_type_stage_expr requires a full resolver pass and an EvalContext
    // that may not be available at annotation resolution time.
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    match name.as_str() {
                        "or" | "union" => {
                            let mut members = Vec::with_capacity(entries.len() - 1);
                            for entry in &entries[1..] {
                                if entry.node.key.is_some() {
                                    continue;
                                }
                                members.push(
                                    Box::pin(resolve_type_expr(
                                        &entry.node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?,
                                );
                            }
                            if !members.is_empty() {
                                return Ok(typevalue_normalize_union_ctx(
                                    members,
                                    Some(&state.ctx),
                                ));
                            }
                        }
                        "all" => {
                            let mut members = Vec::with_capacity(entries.len() - 1);
                            for entry in &entries[1..] {
                                if entry.node.key.is_some() {
                                    continue;
                                }
                                members.push(
                                    Box::pin(resolve_type_expr(
                                        &entry.node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?,
                                );
                            }
                            if !members.is_empty() {
                                return Ok(typevalue_normalize_intersection(members));
                            }
                        }
                        "without" => {
                            if let Some(inner_entry) = entries.get(1) {
                                if inner_entry.node.key.is_none() {
                                    let inner = Box::pin(resolve_type_expr(
                                        &inner_entry.node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?;
                                    return Ok(make_typevalue_negation(inner));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Unified type-head application: [Name Arg1 Arg2 ...] or bare [Name].
    //
    // Routes through `resolve_type_head` which checks the unified scope chain:
    //   annotation_scope (kind_env entries) → type_stage_scope (all types/classes/tycons)
    //
    // Unified type-head resolution checks all four registries in order:
    //   - type_stage_scope  → resolved TypeValue (Integer, DirCap, user TyCon, etc.)
    //   - type_stage_fns    → parameterized constructor thunk (Seq, Map, etc.)
    //   - type_stage_type_vars → kind-annotated TypeVar (Operator, Label, m@Operator)
    //   - env class registry → class constraint (Iterable, Comparable, etc.)
    // All cases go through the single canonical lookup path in resolve_type_head.
    //
    // IMPORTANT: We only enter this block when the name is recognizable as a type head
    // (found in class_env, tycon_env, or kind_env as an Operator-kinded name). Uppercase
    // names that are NOT recognized by any of these environments fall through to the
    // nominal variant constructor block below (e.g., `[Ok a]` where Ok is a user variant).
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    // Check whether this name is a known type head — i.e., it appears in
                    // type_stage_scope, type_stage_fns, type_stage_type_vars, or class env.
                    let in_scope = state
                        .type_stage_data
                        .scope
                        .iter()
                        .any(|s| s.contains_key(name.as_str()))
                        || state.type_stage_data.fns.contains_key(name.as_str())
                        || state.type_stage_data.type_vars.contains_key(name.as_str())
                        || state.classes.iter().any(|c| c.name == name.as_str());

                    if in_scope {
                        // Resolve argument types from the remaining positional entries.
                        // Keyed entries (constraint:, doc:, bind:, etc.) are metadata — skip them.
                        let mut resolved_args: Vec<TypeValue> = Vec::new();
                        for entry in entries[1..].iter() {
                            if entry.node.key.is_some() {
                                continue;
                            }
                            resolved_args.push(
                                resolve_type_expr(
                                    &entry.node.value,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                )
                                .await?,
                            );
                        }

                        // Delegate to the unified lookup with resolved args.
                        return Box::pin(resolve_type_head(
                            name,
                            &resolved_args,
                            state,
                            constraints,
                            span,
                        ))
                        .await;
                    }
                }
            }
        }
    }

    // Named uppercase-key constructor form: `[type File: [path: String] Noop]`
    //
    // Detected when ANY entry has a named key whose name starts with an uppercase letter.
    // In this form:
    //   - Named entry with uppercase-first key → payload constructor:
    //       `File: [path: String]` → NominalVariant { tycon: "TypeName", ctor: "File", fields: {path: String} }
    //   - Positional entry with bare uppercase VarRef → unit constructor:
    //       `Noop` → NominalVariant { tycon: "TypeName", ctor: "Noop", fields: {} }
    //
    // Payload constructors use named uppercase keys: `[File: [path: String]]`.
    // The positional-head form `[File path: String]` is a parse error.
    {
        let has_named_uppercase_key = entries.iter().any(|e| {
            if let Some(k) = &e.node.key {
                matches!(&k.expr, SurfaceExpression::VarRef { name, .. }
                    if crate::eval::is_constructor_name(name))
            } else {
                false
            }
        });

        if has_named_uppercase_key {
            let mut members: Vec<TypeValue> = Vec::with_capacity(entries.len());
            for entry in entries {
                match &entry.node.key {
                    Some(key_node) => {
                        // Named entry: uppercase key is the constructor name, value is payload.
                        let ctor_name = match &key_node.expr {
                            SurfaceExpression::VarRef { name, .. } => name.clone(),
                            _ => {
                                return Err(Diagnostic::error(
                                    KIND,
                                    "named constructor key must be a bare uppercase word",
                                    key_node.span.clone(),
                                ))
                            }
                        };
                        if !crate::eval::is_constructor_name(&ctor_name) {
                            return Err(Diagnostic::error(KIND,
                                format!(
                                    "constructor name must start with an uppercase letter, got `{ctor_name}`"
                                ),
                                key_node.span.clone(),
                            ));
                        }
                        // Resolve the payload dict. The value should be a Dict of named fields.
                        let variant_fields: indexmap::IndexMap<String, Arc<crate::value::Value>> =
                            match &entry.node.value.expr {
                                SurfaceExpression::Dict {
                                    entries: field_entries,
                                    scope: _,
                                } => {
                                    let mut fields = indexmap::IndexMap::new();
                                    for fe in field_entries {
                                        let field_name = match &fe.node.key {
                                            Some(k) => match &k.expr {
                                                SurfaceExpression::StringLiteral {
                                                    content: s,
                                                    ..
                                                } => s.clone(),
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    name.clone()
                                                }
                                                _ => {
                                                    return Err(Diagnostic::error(KIND,
                                                        "payload field names must be bare words",
                                                        k.span.clone(),
                                                    ))
                                                }
                                            },
                                            None => {
                                                return Err(Diagnostic::error(KIND,
                                                    "payload constructor fields must be named (e.g. `path: String`)",
                                                    fe.span.clone(),
                                                ))
                                            }
                                        };
                                        let field_ty = Box::pin(resolve_type_expr(
                                            &fe.node.value,
                                            state,
                                            constraints,
                                            ann_mapping,
                                            row_ann_mapping,
                                            type_params_scope,
                                        ))
                                        .await?;
                                        fields.insert(field_name, field_ty);
                                    }
                                    fields
                                }
                                _ => {
                                    // Single non-dict value as payload — resolve as positional field "0".
                                    let payload_ty = Box::pin(resolve_type_expr(
                                        &entry.node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    ))
                                    .await?;
                                    let mut fields = indexmap::IndexMap::new();
                                    fields.insert("0".to_string(), payload_ty);
                                    fields
                                }
                            };
                        members.push(make_typevalue_nominal_variant(
                            tycon_name,
                            &ctor_name,
                            make_typevalue_record(variant_fields, None),
                        ));
                    }
                    None => {
                        // Positional entry: must be a bare uppercase VarRef (unit constructor).
                        match &entry.node.value.expr {
                            SurfaceExpression::VarRef { name, .. }
                                if crate::eval::is_constructor_name(name) =>
                            {
                                members.push(make_typevalue_nominal_variant(
                                    tycon_name,
                                    name,
                                    make_typevalue_record(indexmap::IndexMap::new(), None),
                                ));
                            }
                            _ => {
                                return Err(Diagnostic::error(KIND,
                                    "in a mixed constructor body, positional entries must be bare uppercase unit constructor names (e.g. `Noop`)",
                                    entry.span.clone(),
                                ));
                            }
                        }
                    }
                }
            }
            return Ok(if members.len() == 1 {
                members.into_iter().next().unwrap()
            } else {
                typevalue_normalize_union_ctx(members, Some(&state.ctx))
            });
        }
    }

    // Nominal variant constructor: [Constructor payload-type] or [Constructor field: Type ...]
    // Matches either form:
    // - Pure positional with uppercase first entry (e.g., [Ok a], [None]):
    //   First entry is constructor tag, optional second entry is payload type
    // - Mixed positional+keyed with uppercase first entry (e.g., [MyOk n: Int]):
    //   First positional is constructor tag, keyed entries are named fields
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            // Check if first entry is positional (auto-indexed)
            if first.node.key.is_none() {
                // Extract the constructor tag name from VarRef or Annotated (T-1052).
                // `[Constructor field: Type ...]` — first entry is VarRef.
                // `[Constructor@[as-type: fn  guarding: false] field: Type ...]` — first entry
                // is Annotated { name: "Constructor", annotation: PropertyDict([...]) }.
                // The annotation carries type-level metadata; the name is the constructor tag.
                // Both forms resolve identically for type-checking purposes; T-1053 reads
                // the annotation from the SurfaceNode tree to populate FnAnnotation.extra.
                // Both plain and annotated VarRef use the name field.
                let tag_opt: Option<String> = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                    _ => None,
                };
                if let Some(tag) = tag_opt {
                    // `tag` is an owned String; pass as &str where needed.
                    // Check if tag is uppercase (constructor name).
                    // BUT: builtin type names (Int, Float, String, Bool, Number, etc.) also
                    // start with uppercase and must NOT be treated as NominalVariant.
                    // Resolve builtin type names through resolve_type_name first.
                    let is_builtin_type = state
                        .type_name_to_id
                        .get(tag.as_str())
                        .and_then(|id| state.tycon_env.get(id))
                        .is_some();
                    if entries.len() == 1 && first.node.key.is_none() {
                        // Single positional entry: try resolve_type_name first.
                        // This handles builtin type names (Int, Float, String, etc.) and
                        // user-defined type aliases regardless of tycon_env population.
                        // Only fall through to NominalVariant if resolve_type_name fails.
                        let row_ref: Option<&HashMap<String, String>> =
                            row_ann_mapping.as_ref().map(|m| &**m);
                        let name_result = resolve_type_name(
                            &tag,
                            span.clone(),
                            state,
                            constraints,
                            ann_mapping,
                            type_params_scope,
                            &row_ref,
                        )
                        .await;
                        match name_result {
                            Ok(ty) => return Ok(ty),
                            // "type-check" means the name is not a known type in scope;
                            // fall through to the NominalVariant check below.
                            Err(e) if e.message.starts_with("undefined type:") => {}
                            // Any other error (e.g., row-variable collision, strict-mode
                            // parameter violation) is a genuine type diagnostic — propagate it.
                            Err(e) => return Err(e),
                        }
                    }
                    if crate::eval::is_constructor_name(&tag) && !is_builtin_type {
                        // Case 1: Pure positional — [Constructor] or [Constructor PayloadType]
                        let all_remaining_positional =
                            entries[1..].iter().all(|e| e.node.key.is_none());
                        if all_remaining_positional {
                            if entries.len() == 1 {
                                return Ok(make_typevalue_nominal_variant(
                                    tycon_name,
                                    &tag,
                                    make_typevalue_record(indexmap::IndexMap::new(), None),
                                ));
                            } else if entries.len() == 2 {
                                // Check if second entry is ALSO an uppercase constructor name.
                                // If so, this is a union of two unit constructors [type True False],
                                // not a single-payload constructor [Ok a].
                                // Extract the second entry's tag name (if it's a VarRef or Annotated).
                                let second_tag_opt: Option<String> =
                                    match &entries[1].node.value.expr {
                                        // Both plain and annotated VarRef use the name field.
                                        SurfaceExpression::VarRef { name, .. } => {
                                            Some(name.clone())
                                        }
                                        _ => None,
                                    };
                                let second_is_constructor = second_tag_opt
                                    .as_ref()
                                    .is_some_and(|name| crate::eval::is_constructor_name(name));

                                if second_is_constructor {
                                    // Both entries are uppercase constructor names.
                                    // Fall through to the multi-entry union path.
                                    // Example: [type True False] → Union(True, False)
                                } else {
                                    // Single-payload constructor: [Ok a]
                                    let payload_ty = resolve_type_expr(
                                        &entries[1].node.value,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    )
                                    .await?;
                                    let mut fields = indexmap::IndexMap::new();
                                    fields.insert("0".to_string(), payload_ty);
                                    return Ok(make_typevalue_nominal_variant(
                                        tycon_name,
                                        &tag,
                                        make_typevalue_record(fields, None),
                                    ));
                                }
                            }
                            // 3+ all-positional entries: not a constructor with positional payload.
                            // Fall through to the multi-entry union path below.
                            // Example: `[type Shape [Circle r: Int] [Square s: Int]]` produces
                            // body `[Shape [Circle r: Int] [Square s: Int]]` — all three are
                            // union members, not a constructor tag + 2 payloads.
                        } else {
                            // Case 2: Mixed positional+keyed — [Constructor field: Type ...]
                            // First entry is tag (positional), remaining are named fields (keyed).
                            // Only reached when some of entries[1..] are keyed.
                            let mut variant_fields = indexmap::IndexMap::new();
                            for field_entry in &entries[1..] {
                                match &field_entry.node.key {
                                    Some(k) => {
                                        // Field name from bare word key or annotated key (T-1052).
                                        // `field: Type` → key is Str("field").
                                        // `field@Child: Type` → key is Annotated { name: "field", ... }.
                                        // The annotation is stored in the SurfaceNode tree for T-1053;
                                        // type resolution uses only the name.
                                        let field_name = match &k.expr {
                                            SurfaceExpression::StringLiteral {
                                                content: s, ..
                                            } => s.clone(),
                                            // Both plain and annotated VarRef use the name field.
                                            SurfaceExpression::VarRef { name, .. } => name.clone(),
                                            _ => return Err(Diagnostic::error(
                                                KIND,
                                                "nominal variant field names must be bare words",
                                                k.span.clone(),
                                            )),
                                        };
                                        let field_ty = resolve_type_expr(
                                            &field_entry.node.value,
                                            state,
                                            constraints,
                                            ann_mapping,
                                            row_ann_mapping,
                                            type_params_scope,
                                        )
                                        .await?;
                                        variant_fields.insert(field_name, field_ty);
                                    }
                                    None => {
                                        return Err(Diagnostic::error(KIND,
                                            "nominal variant constructor with named fields requires all fields after the constructor tag to be keyed (field: Type)",
                                            field_entry.span.clone(),
                                        ));
                                    }
                                }
                            }
                            return Ok(make_typevalue_nominal_variant(
                                tycon_name,
                                &tag,
                                make_typevalue_record(variant_fields, None),
                            ));
                        }
                    }
                }
            }
        }
    }

    // All-positional entries where every member is a bare uppercase VarRef → union of unit
    // constructors. This is the type-body case: `[type True False]` body is
    // `Dict([VarRef("True"), VarRef("False")])` and must produce Union(NominalVariant("True"),
    // NominalVariant("False")). We do NOT call resolve_type_expr on each entry here because
    // the constructors are not yet registered in the environment (chicken-and-egg). Instead,
    // create NominalVariants directly — the same as the single-entry and 2-entry constructor
    // paths above. This path fires when the nominal variant block fell through (both entries
    // are uppercase constructor names and there are 2+ such entries).
    let all_positional = entries.iter().all(|e| e.node.key.is_none());
    if all_positional && entries.len() >= 2 {
        let all_uppercase_varref = entries.iter().all(|e| {
            matches!(&e.node.value.expr, SurfaceExpression::VarRef { name, .. }
                if crate::eval::is_constructor_name(name))
        });
        if all_uppercase_varref {
            let members: Vec<TypeValue> = entries
                .iter()
                .map(|e| {
                    let name = match &e.node.value.expr {
                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                        _ => unreachable!(),
                    };
                    make_typevalue_nominal_variant(
                        tycon_name,
                        &name,
                        make_typevalue_record(indexmap::IndexMap::new(), None),
                    )
                })
                .collect();
            return Ok(typevalue_normalize_union_ctx(members, Some(&state.ctx)));
        }
    }
    if all_positional && entries.len() == 1 {
        if let Some(first) = entries.first() {
            if first.node.key.is_none()
                && !matches!(&first.node.value.expr, SurfaceExpression::VarRef { .. })
            {
                return resolve_type_expr(
                    &first.node.value,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await;
            }
        }
    }

    let mut fields: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    // Column constraint: `{_ : V}` or `{_@K : V}` annotation syntax (T-950).
    // At most one `_` per row type; duplicate produces a type error.
    // uniform_tail: None = closed, Some((key_ty, value_ty)) = Uniform tail
    let mut uniform_tail: Option<(Option<TypeValue>, TypeValue)> = None;

    for entry in entries {
        if let SurfaceExpression::Placeholder(_name, _) = &entry.node.value.expr {
            // BAS: `...` annotations express user intent for openness; under BAS width
            // subtyping all records are closed — is_subtype handles extra fields.
            continue;
        }

        // Column constraint sentinel: key is `_` (bare wildcard) or `_@K` (typed wildcard).
        // Recognized in key position: SurfaceExpression::VarRef { name: "_" } or
        // SurfaceExpression::Annotated { name: "_", annotation: K }.
        // Both plain and annotated VarRef use the name field.
        let is_wildcard_key = match &entry.node.key {
            Some(k) => matches!(&k.expr, SurfaceExpression::VarRef { name, .. } if name == "_"),
            None => false,
        };

        if is_wildcard_key {
            if uniform_tail.is_some() {
                return Err(Diagnostic::error(KIND,
                    "duplicate uniform-field sentinel `_` in row type annotation — at most one `_` allowed per row",
                    entry.span.clone(),
                ));
            }
            let value_ty = resolve_type_expr(
                &entry.node.value,
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await?;
            // Check for typed-key form `_@K` vs plain `_`
            // Check for typed-key form `_@K` (annotated VarRef) vs plain `_`.
            let key_ty = match entry.node.key.as_ref().map(|k| &k.expr) {
                Some(SurfaceExpression::VarRef {
                    annotation: Some(annotation),
                    ..
                }) => {
                    // `_@K`: resolve K as the key type constraint.
                    let key_t = resolve_annotation(
                        &annotation.node,
                        annotation.span.clone(),
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await?;
                    Some(key_t)
                }
                _ => None, // plain `_`: no key type constraint
            };
            uniform_tail = Some((key_ty, value_ty));
            continue;
        }

        let key = match &entry.node.key {
            Some(k) => match &k.expr {
                SurfaceExpression::StringLiteral { content: s, .. } => s.clone(),
                // Both plain and annotated VarRef use the name field.
                // Annotated field key: `field@Child: Type` (T-1052) — annotation is on VarRef.
                SurfaceExpression::VarRef { name, .. } => name.clone(),
                _ => {
                    return Err(Diagnostic::error(
                        KIND,
                        "type record keys must be bare words",
                        k.span.clone(),
                    ))
                }
            },
            None => {
                return Err(Diagnostic::error(
                    KIND,
                    "auto-indexed entries not supported in type expressions",
                    entry.span.clone(),
                ))
            }
        };
        let ty = resolve_type_expr(
            &entry.node.value,
            state,
            constraints,
            ann_mapping,
            row_ann_mapping,
            type_params_scope,
        )
        .await?;
        fields.insert(key, ty);
    }

    // Multi-field record annotations always build a single TypeValue.Record.
    //
    // `@[file: String  start-line: Integer]` → `Record { fields: {file: Str, start-line: Int} }`
    //
    // Previously this split into `Intersection([{file: String}, {start-line: Integer}])`, which
    // caused the type checker to treat the TypeNode constructor names as record field names
    // when resolving `@Dict` (B-663) and produced incorrect subtype errors for property-dict
    // annotations on function parameters.
    //
    // The correct representation is always a single closed Record with all named fields.
    // BAS width subtyping handles structural subtyping without splitting.

    // Build the record TypeValue. uniform_tail encodes the row tail.
    // When a uniform tail is present, wrap it as RowTail.Uniform with the value type
    // and optional key type.
    let tail = match uniform_tail {
        Some((key_ty, val_ty)) => Some(make_rowtail_uniform_with_key_type(val_ty, key_ty)),
        None => None,
    };
    Ok(make_typevalue_record(fields, tail))
}

// ============================================================================
// Type-Stage Evaluation (T-1060, equirecursive-types)
// ============================================================================

/// Synthesize a `SurfaceNode` from PropertyDict entries for type-stage evaluation.
///
/// `Annotation::PropertyDict` entries that come from implied-call bracket expressions
/// (e.g., `@[my-combinator Int String]`) are stored as positional entries where the
/// first entry is a bare VarRef — the function to call. This mirrors the parser
/// conversion in `expression_to_annotation`: `SurfaceExpression::Call { implied: true }`
/// is lowered to a PropertyDict for the annotation resolver.
///
/// For type-stage evaluation we need to reverse this: reconstruct the original Call form
/// so the evaluator sees a function call rather than an integer-keyed dict.
///
/// - **Implied call form** (first entry is positional VarRef, all entries positional):
///   synthesize `SurfaceExpression::Call { implied: true, func, args }`.
///
/// - **Dict form** (keyed entries or first entry is not a VarRef):
///   synthesize `SurfaceExpression::Dict { entries: entries, scope: std::sync::OnceLock::new() }`. Dict evaluation in the type-stage
///   env produces an integer-keyed Dict value, which `typenode_value_to_type` will not
///   recognize — this is the correct fallback for metadata-style annotations that reach
///   the type-stage path (they will return `Type::Unknown` after failing conversion).
fn synthesize_type_stage_node(entries: &[Spanned<SurfaceEntry>], span: Span) -> Arc<SurfaceNode> {
    // Detect implied call: ALL entries are positional (key: None) AND the first entry
    // is a VarRef. This matches the parser rule in expression_to_annotation.
    let is_implied_call = !entries.is_empty()
        && entries.iter().all(|e| e.node.key.is_none())
        && matches!(
            &entries[0].node.value.expr,
            SurfaceExpression::VarRef { .. }
        );

    if is_implied_call {
        let func = Arc::clone(&entries[0].node.value);
        let args: Vec<Arc<SurfaceNode>> = entries[1..]
            .iter()
            .map(|e| Arc::clone(&e.node.value))
            .collect();
        Arc::new(SurfaceNode::new(
            SurfaceExpression::Call {
                func,
                args,
                named_args: vec![],
                implied: true,
                pipe_span: None,
            },
            span,
        ))
    } else {
        Arc::new(SurfaceNode::new(
            SurfaceExpression::Dict {
                entries: entries.to_vec(),
                scope: std::sync::OnceLock::new(),
            },
            span,
        ))
    }
}

/// Extract and materialize the payload dict from a `Value::Variant` with a payload.
///
/// Returns `Ok(None)` if:
/// - The value is not a Variant, or has no payload.
/// - The materialized payload is not a Dict.
///
/// Returns `Err` if payload or field materialization fails — propagates the eval error
/// so callers can surface it as a diagnostic rather than silently dropping it.
///
/// Used by `typenode_value_to_type` to access named fields of structural TypeNode variants.
async fn variant_payload_dict(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> crate::error::DiagnosticResult<Option<HashMap<String, Value>>> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let payload_id = match val {
        Value::Variant {
            payload: Some(id), ..
        } => id.clone(),
        _ => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
    };
    let payload_thunk = payload_id;
    let (mat_result, mat_diags) = crate::eval::materialize(&payload_thunk, None, ctx).await.into_parts();
    _dr_diags.extend(mat_diags);
    let payload_val = match mat_result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };
    match payload_val {
        Value::Dict { entries: dict, .. } => {
            // Extract each string-keyed field, materializing the field value.
            let mut fields = HashMap::new();
            for (key, thunk) in &dict {
                if let HashableValue::Str(k) = key {
                    let (mat_result2, mat_diags2) = crate::eval::materialize(thunk, None, ctx).await.into_parts();
                    _dr_diags.extend(mat_diags2);
                    let v = match mat_result2 {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    };
                    fields.insert(k.to_string(), v);
                }
            }
            crate::error::DiagnosticResult::ok(Some(fields)).with_diagnostics(_dr_diags)
        }
        _ => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
    }
}

/// Collect a TypeNode children Dict (`[Map Int TypeNode]`) into a Vec of `Type`.
///
/// TypeNode fields like `Union.types`, `Intersect.types`, `Arrow.params`, and
/// `TypeApplication.args` are now integer-keyed Dicts of TypeNode values.
/// Each value is converted via `typenode_value_to_type`.
///
/// Returns `Ok(None)` if the input is not a Dict or a TypeNode element is unrecognized.
/// Returns `Err` if any element or its sub-materialization fails — propagates eval errors.
async fn collect_typenode_seq(
    dict_val: Value,
    ctx: &Arc<crate::eval::EvalContext>,
    type_stage_scope: &[std::collections::HashMap<String, crate::type_infer::TypeValue>],
    type_name_to_id: &std::collections::HashMap<String, u64>,
) -> crate::error::DiagnosticResult<Option<Vec<TypeValue>>> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    // Value::Annotated is removed; no unwrapping needed.
    let dict = match dict_val {
        Value::Dict { entries: d, .. } => d,
        _ => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
    };

    let mut result = Vec::new();
    let mut i = 0i64;
    loop {
        match dict.get(&HashableValue::Int(i)) {
            Some(thunk) => {
                let (mat_result, mat_diags) = crate::eval::materialize(thunk, None, ctx).await.into_parts();
                _dr_diags.extend(mat_diags);
                let val = match mat_result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };
                let (tv_result, tv_diags) = Box::pin(typenode_value_to_type(
                    &val,
                    ctx,
                    type_stage_scope,
                    type_name_to_id,
                ))
                .await
                .into_parts();
                _dr_diags.extend(tv_diags);
                match tv_result {
                    Ok(Some(ty)) => {
                        result.push(ty);
                        i += 1;
                    }
                    Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            }
            None => return crate::error::DiagnosticResult::ok(Some(result)).with_diagnostics(_dr_diags),
        }
    }
}

/// Convert a materialized TypeNode value (from type-stage evaluation) into a Rust `Type`.
///
/// Called after evaluating a type-stage expression to convert the resulting runtime
/// `Value` back into a `Type` for use by the type checker.
///
/// Returns `Ok(None)` if the value is not a recognizable TypeNode constructor.
/// Returns `Err` if any field or nested materialization fails — propagates eval errors
/// so callers can surface them as diagnostics rather than silently dropping them.
pub(crate) async fn typenode_value_to_type(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
    type_stage_scope: &[std::collections::HashMap<String, crate::type_infer::TypeValue>],
    type_name_to_id: &std::collections::HashMap<String, u64>,
) -> crate::error::DiagnosticResult<Option<TypeValue>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        match val {
            // TypeValue.* variants — already in TypeValue form: return as-is.
            Value::Variant { type_decl_id, .. } if *type_decl_id == TYPE_ID_TYPE_VALUE => {
                crate::error::DiagnosticResult::ok(Some(Arc::new(val.clone()))).with_diagnostics(_dr_diags)
            }

            // TypeNode.* variants — convert to TypeValue.
            Value::Variant {
                ctor_index,
                type_decl_id: variant_tdid,
                ..
            } if *variant_tdid == TYPE_ID_TYPE_NODE => {
                match *ctor_index {
                    TN_UNKNOWN => crate::error::DiagnosticResult::ok(Some(make_typevalue_unknown())).with_diagnostics(_dr_diags),
                    // Top is the sound lattice top (τ <: Top for all τ).
                    TN_TOP => crate::error::DiagnosticResult::ok(Some(make_typevalue_top())).with_diagnostics(_dr_diags),
                    TN_NEVER => crate::error::DiagnosticResult::ok(Some(make_typevalue_never())).with_diagnostics(_dr_diags),
                    // Absent: the empty closed dict type — the Rust-protocol encoding of the absent/null sentinel.
                    // The empty dict `[]` is Value::Dict({}) at runtime; @Absent is its static type.
                    TN_ABSENT => crate::error::DiagnosticResult::ok(Some(make_typevalue_record(indexmap::IndexMap::new(), None))).with_diagnostics(_dr_diags),
                    // Primitive leaf types — no payload, direct mapping to TypeValue.Repr variants.
                    // These appear as field values in TypeNode.Dict payloads (e.g., the
                    // "fields" dict of a Dict TypeNode) and as union/intersection members.
                    TN_INT => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_INT))).with_diagnostics(_dr_diags),
                    TN_FLOAT => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_FLOAT))).with_diagnostics(_dr_diags),
                    TN_STRING => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_STRING))).with_diagnostics(_dr_diags),
                    TN_BYTES => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_BYTES))).with_diagnostics(_dr_diags),
                    TN_PROXY => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_PROXY))).with_diagnostics(_dr_diags),
                    // Any callable — variadic function with zero required fixed params and unknown
                    // return type. Variadic=true means any number of arguments is accepted; the
                    // arity check skips the "too many arguments" path for variadic functions.
                    // This is DISTINCT from TypeValue.Unknown: Callable asserts the value IS a
                    // function; Unknown means we know nothing about the value at all.
                    TN_CALLABLE => crate::error::DiagnosticResult::ok(Some(make_typevalue_fn_with_flags(
                        vec![],
                        make_typevalue_unknown(),
                        Some(0), // required_count — no fixed params
                        true,    // variadic — accepts any number of arguments
                        Vec::new(),
                    ))).with_diagnostics(_dr_diags),

                    // ── Union ─────────────────────────────────────────────────────────────
                    // TypeNode.Union { types: [Seq TypeNode] } → Type::normalize_union(members)
                    TN_UNION => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let types_val = match fields.get(TN_FIELD_TYPES) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let (cts_result, cts_diags) = collect_typenode_seq(
                            types_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(cts_diags);
                        let members = match cts_result {
                            Ok(Some(v)) => v,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        if members.is_empty() {
                            return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags); // Empty union is ill-formed — fall back to Unknown.
                        }
                        crate::error::DiagnosticResult::ok(Some(typevalue_normalize_union(members))).with_diagnostics(_dr_diags)
                    }

                    // ── Intersect ────────────────────────────────────────────────────────
                    // TypeNode.Intersect { types: [Seq TypeNode] } → typevalue_normalize_intersection(members)
                    TN_INTERSECT => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let types_val = match fields.get(TN_FIELD_TYPES) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let (cts_result, cts_diags) = collect_typenode_seq(
                            types_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(cts_diags);
                        let members = match cts_result {
                            Ok(Some(v)) => v,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        if members.is_empty() {
                            return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags); // Empty intersection is ill-formed — fall back to Unknown.
                        }
                        crate::error::DiagnosticResult::ok(Some(typevalue_normalize_intersection(members))).with_diagnostics(_dr_diags)
                    }

                    // ── Negation ─────────────────────────────────────────────────────────
                    // TypeNode.Negation { inner: TypeNode } → make_typevalue_negation(inner)
                    TN_NEGATION => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let inner_val = match fields.get(TN_FIELD_INNER) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let (tv_result, tv_diags) = typenode_value_to_type(
                            &inner_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(tv_diags);
                        let inner_type = match tv_result {
                            Ok(Some(t)) => t,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_negation(inner_type))).with_diagnostics(_dr_diags)
                    }

                    // ── Dict ─────────────────────────────────────────────────────────────
                    // TypeNode.Dict { fields: [Map String TypeNode], open: Bool,
                    //                 key-type?: TypeNode, value-type?: TypeNode }
                    // → Type::Dict(Row { fields: BTreeMap<String, Type>, tail: Empty | Uniform })
                    //
                    // Three tail cases:
                    //   key-type present → Uniform { key: Some(K), value: V }  (typed Map[K:V])
                    //   open: 1, no key-type → Uniform { key: None, value: Any } (open record)
                    //   open: 0, no key-type → Empty                            (closed / Null)
                    TN_DICT => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let payload_fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let fields_val = match payload_fields.get(TN_FIELD_FIELDS) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let open_val = match payload_fields.get(TN_FIELD_OPEN) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };

                        // `fields` is a Dict (Map String TypeNode) — string-keyed, values are TypeNodes.
                        let record_fields = match fields_val {
                            Value::Dict {
                                entries: ref dict, ..
                            } => {
                                let mut out: indexmap::IndexMap<String, TypeValue> =
                                    indexmap::IndexMap::new();
                                for (key, thunk) in dict {
                                    if let HashableValue::Str(k) = key {
                                        let (mat_result, mat_diags) = crate::eval::materialize(thunk, None, ctx).await.into_parts();
                                        _dr_diags.extend(mat_diags);
                                        let v = match mat_result {
                                            Ok(v) => v,
                                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                        };
                                        let (tv_result, tv_diags) = typenode_value_to_type(
                                            &v,
                                            ctx,
                                            type_stage_scope,
                                            type_name_to_id,
                                        )
                                        .await
                                        .into_parts();
                                        _dr_diags.extend(tv_diags);
                                        let ty = match tv_result {
                                            Ok(Some(t)) => t,
                                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                        };
                                        out.insert(k.to_string(), ty);
                                    }
                                }
                                out
                            }
                            // fields is not a Dict — TypeNode.Dict payload is malformed; cannot convert.
                            _ => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };

                        // Optional key-type: and value-type: fields enable typed-key map encoding.
                        // These are the protocol-defined field names per builtin_core.llt:33.
                        // key-type is resolved and stored in the RowTail.Uniform payload.
                        let tail =
                            if let Some(kt_val) = payload_fields.get(TN_FIELD_KEY_TYPE).cloned() {
                                // key-type: present — resolve it to a TypeValue.
                                let (tv_result, tv_diags) = typenode_value_to_type(
                                    &kt_val,
                                    ctx,
                                    type_stage_scope,
                                    type_name_to_id,
                                )
                                .await
                                .into_parts();
                                _dr_diags.extend(tv_diags);
                                let key_ty = match tv_result {
                                    Ok(Some(t)) => t,
                                    Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                };
                                // value-type: defaults to Top when absent.
                                let value_ty = if let Some(vt_val) =
                                    payload_fields.get(TN_FIELD_VALUE_TYPE).cloned()
                                {
                                    let (tv_result2, tv_diags2) = typenode_value_to_type(
                                        &vt_val,
                                        ctx,
                                        type_stage_scope,
                                        type_name_to_id,
                                    )
                                    .await
                                    .into_parts();
                                    _dr_diags.extend(tv_diags2);
                                    match tv_result2 {
                                        Ok(Some(t)) => t,
                                        Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                    }
                                } else {
                                    make_typevalue_top()
                                };
                                Some(make_rowtail_uniform_with_key_type(value_ty, Some(key_ty)))
                            } else if matches!(&open_val, Value::Int { n, .. } if *n != 0) {
                                // Open record: any field value is allowed — RowTail.Uniform with Top.
                                Some(make_rowtail_uniform(make_typevalue_top()))
                            } else {
                                // Closed record
                                None
                            };

                        if let Some(Value::Int { n, .. }) = payload_fields.get("repr") {
                            if *n > 0 {
                                return crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(*n as u64))).with_diagnostics(_dr_diags);
                            }
                        }
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_record(record_fields, tail))).with_diagnostics(_dr_diags)
                    }

                    // ── Arrow ─────────────────────────────────────────────────────────────
                    // TypeNode.Arrow { params: [Seq TypeNode], result: TypeNode }
                    // → Type::Function { params: Vec<(None, Type)>, ret: Box<Type>, variadic: false }
                    TN_ARROW => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let params_val = match fields.get(TN_FIELD_PARAMS) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let result_val = match fields.get(TN_FIELD_RESULT) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };

                        let (cts_result, cts_diags) = collect_typenode_seq(
                            params_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(cts_diags);
                        let param_types = match cts_result {
                            Ok(Some(v)) => v,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let (tv_result, tv_diags) = typenode_value_to_type(
                            &result_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(tv_diags);
                        let ret_type = match tv_result {
                            Ok(Some(t)) => t,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };

                        let param_types_named: Vec<(Option<String>, TypeValue, crate::ast::Span)> =
                            param_types.into_iter().map(|ty| (None, ty, crate::rust_span!())).collect();
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_fn(param_types_named, ret_type))).with_diagnostics(_dr_diags)
                    }

                    // ── TypeConstructor ───────────────────────────────────────────────────
                    // TypeNode.TypeConstructor { name: String }
                    // Bare (transient): name without '.' → Type::TyCon(name) for expansion
                    // Qualified (leaf): name with '.' → Type::NominalVariant or TyCon leaf
                    TN_TYPE_CONSTRUCTOR => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        // Look up the name in the type-stage scope chain.
                        // The type-stage scope holds resolved TypeValues directly.
                        // This covers primitive aliases (Integer, Float, String, etc.) and any
                        // user-defined or prelude-defined type-stage names.
                        for scope in type_stage_scope {
                            if let Some(tv) = scope.get(&name) {
                                return crate::error::DiagnosticResult::ok(Some(tv.clone())).with_diagnostics(_dr_diags);
                            }
                        }
                        // Not found in the type-stage scope — treat as an opaque TyCon.
                        // This is correct for types declared in the runtime section (e.g., Color,
                        // BuilderHandle) that are not yet in the type-stage scope chain.
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(
                            type_name_to_id.get(name.as_str()).copied().unwrap_or(0),
                        ))).with_diagnostics(_dr_diags)
                    }

                    // ── TypeApplication ───────────────────────────────────────────────────
                    // TypeNode.TypeApplication { ctor: TypeNode, args: [Seq TypeNode] }
                    // → left-associative Type::App chain: App(App(ctor, args[0]), args[1])...
                    TN_TYPE_APPLICATION => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let ctor_val = match fields.get(TN_FIELD_CTOR) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let args_val = match fields.get(TN_FIELD_ARGS) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };

                        let (tv_result, tv_diags) = typenode_value_to_type(
                            &ctor_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(tv_diags);
                        let ctor_type = match tv_result {
                            Ok(Some(t)) => t,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let (cts_result, cts_diags) = collect_typenode_seq(
                            args_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        )
                        .await
                        .into_parts();
                        _dr_diags.extend(cts_diags);
                        let arg_types = match cts_result {
                            Ok(Some(v)) => v,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };

                        if arg_types.is_empty() {
                            // Zero-arg application — return the constructor itself.
                            return crate::error::DiagnosticResult::ok(Some(ctor_type)).with_diagnostics(_dr_diags);
                        }

                        // Build left-associative App chain.
                        let mut result = ctor_type;
                        for arg in arg_types {
                            result = make_typevalue_app(result, arg);
                        }
                        crate::error::DiagnosticResult::ok(Some(result)).with_diagnostics(_dr_diags)
                    }

                    // ── TypeVar ───────────────────────────────────────────────────────────
                    // TypeNode.TypeVar { name: String, kind: String }
                    // → TypeValue.Var { name }  (level lives in InferenceContext, not in TypeValue)
                    TN_TYPE_VAR => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        // Level is not stored in TypeValue.Var — it lives in InferenceContext.
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_var(&name))).with_diagnostics(_dr_diags)
                    }

                    // ── Recursive ────────────────────────────────────────────────────────
                    // TypeNode.Recursive { var: String, body: TypeNode }
                    // → TypeValue.Recursive { body: TypeValue } via make_typevalue_recursive(body)
                    //
                    // The `var` field names the μ-binder (e.g., "𝜇List"). TypeNode.RecursiveRef
                    // nodes in the body map to TypeValue.Var (same name as the μ-binder) — this
                    // is the named-variable approach rather than de Bruijn RecursiveRef. The var
                    // is treated as a non-generalizable TypeVar during inference (never added to
                    // ctx.levels for generalization).
                    TN_RECURSIVE => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        // The "var" field names the binder but is not needed for de Bruijn
                        // representation — only its presence is required for a well-formed
                        // TypeNode.Recursive payload.
                        if !fields.contains_key(TN_FIELD_VAR) {
                            return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags);
                        }
                        let body_val = match fields.get(TN_FIELD_BODY) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let (tv_result, tv_diags) = Box::pin(typenode_value_to_type(
                            &body_val,
                            ctx,
                            type_stage_scope,
                            type_name_to_id,
                        ))
                        .await
                        .into_parts();
                        _dr_diags.extend(tv_diags);
                        let body = match tv_result {
                            Ok(Some(t)) => t,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_recursive(body))).with_diagnostics(_dr_diags)
                    }

                    // ── RecursiveRef ──────────────────────────────────────────────────────
                    // TypeNode.RecursiveRef { name: String }
                    // → TypeValue.Var { name }  (de Bruijn back-reference at depth 0)
                    //
                    // RecursiveRef is a leaf node marking a back-reference to the enclosing
                    // TypeNode.Recursive binder. In the TypeValue representation, this is
                    // TypeValue.Var with the same binder name — the InferenceContext tracks
                    // that this var is a recursive self-reference (never generalized).
                    TN_RECURSIVE_REF => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        // RecursiveRef maps to TypeValue.Var — same name as the μ-binder.
                        // The InferenceContext knows this var is a recursive self-reference.
                        crate::error::DiagnosticResult::ok(Some(make_typevalue_var(&name))).with_diagnostics(_dr_diags)
                    }

                    // ── IntLiteral ────────────────────────────────────────────────────────
                    // TypeNode.IntLiteral { n: Int } → Type::IntLiteral(n)
                    // Produced by union/or when a raw integer value is normalized via
                    // _wrap-typenode-value in the type-stage prelude (e.g. `[or 0 1]`).
                    TN_INT_LITERAL => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let n_val = match fields.get(TN_FIELD_N) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        match n_val {
                            Value::Int { n, .. } => crate::error::DiagnosticResult::ok(Some(make_typevalue_int_lit(n))).with_diagnostics(_dr_diags),
                            _ => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        }
                    }

                    // ── FloatLiteral ──────────────────────────────────────────────────────
                    // TypeNode.FloatLiteral { value: Float } → TypeValue.FloatLit
                    TN_FLOAT_LITERAL => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let v_val = match fields.get(TN_FIELD_FLOAT_VALUE) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        match v_val {
                            Value::Float { n, .. } => crate::error::DiagnosticResult::ok(Some(make_typevalue_float_lit(n))).with_diagnostics(_dr_diags),
                            _ => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        }
                    }

                    // ── StringLiteral ─────────────────────────────────────────────────────
                    // TypeNode.StringLiteral { s: String } → TypeValue.StrLit
                    TN_STRING_LITERAL => {
                        let (vpd_result, vpd_diags) = variant_payload_dict(val, ctx).await.into_parts();
                        _dr_diags.extend(vpd_diags);
                        let fields = match vpd_result {
                            Ok(Some(f)) => f,
                            Ok(None) => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        let s_val = match fields.get(TN_FIELD_S) {
                            Some(v) => v.clone(),
                            None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        };
                        match s_val.as_str() {
                            Some(s) => crate::error::DiagnosticResult::ok(Some(make_typevalue_str_lit(s))).with_diagnostics(_dr_diags),
                            None => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                        }
                    }

                    // Unknown tag — not a recognized TypeNode constructor.
                    _ => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
                }
            }

            // Raw Value::Int in type position — produced when a literal integer appears
            // directly in a TypeNode sequence (e.g., inside TypeNode.Union.types).
            // S-991: literals in type position produce IntLit/StrLit/FloatLit TypeValues.
            Value::Int { n, .. } => crate::error::DiagnosticResult::ok(Some(make_typevalue_int_lit(*n))).with_diagnostics(_dr_diags),
            Value::String {
                source, start, end, ..
            } => crate::error::DiagnosticResult::ok(Some(make_typevalue_str_lit(&source[*start..*end]))).with_diagnostics(_dr_diags),
            // Raw Value::Float in type position — produced when a float literal appears
            // directly in a TypeNode sequence (e.g., @[or 1.5 2.5] in type-stage evaluation).
            // Parallel to Value::Int handling above.
            Value::Float { n, .. } => crate::error::DiagnosticResult::ok(Some(make_typevalue_float_lit(*n))).with_diagnostics(_dr_diags),

            // Constructor dict — a tinct ADT declaration like `Color: [type Red Green Blue]`
            // evaluates to `{ Red: Variant("Color.Red"), Green: Variant("Color.Green"), ... }`.
            // Detect the pattern: all values are Variants sharing the same qualified prefix.
            // If so, return TypeValue.Op(prefix) — the name of the declared type.
            //
            // When a non-ADT dict is produced by type-stage evaluation for an `@Dict`
            // annotation (e.g., the TypeNode ADT dict when thunks are unsettled, or any other
            // raw Value::Dict appearing in TypeNode position), treat it as the plain Dict type
            // (TypeValue.Repr{repr:"Value::Dict"}) rather than returning Ok(None) which causes
            // downstream callers to interpret the dict's entries as record field types.
            Value::Dict { entries, .. } if !entries.is_empty() => {
                let mut shared_type_decl_id: Option<u64> = None;
                let mut all_match = true;
                for (_key, thunk) in entries {
                    match thunk.peek_result() {
                        Some(Ok(val)) => {
                            match val {
                                Value::Variant { type_decl_id, .. } => match shared_type_decl_id {
                                    None => shared_type_decl_id = Some(*type_decl_id),
                                    Some(existing) if existing == *type_decl_id => {}
                                    _ => {
                                        all_match = false;
                                        break;
                                    }
                                },
                                // Function entries (payload constructors) — still count as the same ADT
                                Value::Function { .. } | Value::Builtin { .. } => {}
                                _ => {
                                    all_match = false;
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()).with_diagnostics(_dr_diags),
                        None => {
                            // Thunk not yet materialized — can't determine
                            all_match = false;
                            break;
                        }
                    }
                }
                if all_match {
                    match shared_type_decl_id {
                        Some(id) => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(id))).with_diagnostics(_dr_diags),
                        None => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_DICT))).with_diagnostics(_dr_diags),
                    }
                } else {
                    // Not an ADT-constructor dict — this is a plain dict value in TypeNode
                    // position (e.g., the Dict from the TypeNode type-stage scope when some
                    // thunks are unsettled). Treat as the plain Dict type.
                    crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_DICT))).with_diagnostics(_dr_diags)
                }
            }

            // Empty Value::Dict in TypeNode position — treat as the plain Dict type.
            // This covers the case where an empty dict is the result of type-stage evaluation
            // for an @Dict annotation.
            Value::Dict { .. } => crate::error::DiagnosticResult::ok(Some(make_typevalue_nominal(crate::value::TYPE_ID_DICT))).with_diagnostics(_dr_diags),

            // Not a recognizable TypeNode or ADT value.
            _ => crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
        }
    })
    .await
}

/// Dispatch a TypeNode `Value` through the type-stage [`AS_TYPENODE_PROTOCOL_NAME`] protocol
/// function, normalizing TypeNode values produced by type-stage expression evaluation using
/// user-defined `as-type:` annotations on TypeNode constructors.  The name `as-typenode` is
/// a **protocol contract** (D-7): Rust requires this exact name in the type-stage map; the
/// prelude is responsible for providing it.  This is not a prelude-specific hack — any
/// compliant prelude must export a function named `as-typenode` that accepts a TypeNode value
/// and returns a resolved Type.
///
/// ## When to call
///
/// Call `as_type_dispatch` when `typenode_value_to_type` returns `None` — i.e., when a
/// TypeNode value has an unrecognised tag (typically a user-defined or prelude-defined
/// constructor such as `TypeNode.Bool` or `TypeNode.SizedBytes`).  Rather than failing
/// with an unrecognised-tag error, the caller should first try `as_type_dispatch` to
/// let the type-stage normalise the value to an existing form.
///
/// ## Mechanism
///
/// 1. Looks up [`AS_TYPENODE_PROTOCOL_NAME`] in `state.type_stage_data.fns` (Function thunks).
/// 2. Materialises the thunk via `state.eval_ctx` to obtain the resolver function value.
/// 3. Invokes the function inline via `evaluate_resolver_by_value` with the TypeNode value as
///    the sole argument. Returns a normalised TypeNode.
/// 4. Converts the normalised TypeNode value to a TypeValue via `typenode_value_to_type`.
///
/// Returns `Ok(None)` if:
/// - `state.type_stage_data.fns` does not contain `as-typenode` (no type-stage configured).
/// - `state.eval_ctx` is `None`.
/// - The resolver function is not a single-parameter `Value::Function`.
/// - The result is unrecognised (would cause recursion; stopped at this depth).
///
/// Emits a `protocol-violation` diagnostic (error level) and returns `Ok(None)` if the
/// type-stage scope is non-empty but does not define [`AS_TYPENODE_PROTOCOL_NAME`] — this
/// indicates a protocol violation: the prelude is not D-7 compliant.
///
/// Returns `Err` if any evaluation step fails — resolver thunk materialisation,
/// function invocation, result thunk materialisation, or `typenode_value_to_type`
/// field materialisation. All eval errors propagate; callers convert them to
/// `Diagnostic` via `.map_err`.
///
/// ## Protocol contract (Axiom 1 / D-7)
///
/// `as-typenode` is an Axiom 1 protocol entry — the Rust type checker requires that the
/// active prelude provides a function named `as-typenode` that accepts TypeNode values and
/// returns their resolved Type.  This is analogous to `tmpl`/`unindent` for strings (D-3):
/// Rust defines the protocol; the prelude implements it.  A custom prelude must provide an
/// `as-typenode` function under this exact name to participate in TypeNode dispatch.
/// Tracked as decision D-7.
pub(crate) async fn as_type_dispatch(
    val: &Value,
    state: &mut InferState,
) -> crate::error::DiagnosticResult<Option<TypeValue>> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    // Step 1: locate the `AS_TYPENODE_PROTOCOL_NAME` resolver in type_stage_fns.
    // This function is a parameterized type constructor that dispatches TypeNode values.
    let resolver_thunk = {
        let scope_non_empty = !state.type_stage_data.fns.is_empty()
            || state.type_stage_data.scope.iter().any(|s| !s.is_empty())
            || !state.type_stage_data.type_vars.is_empty();
        match state
            .type_stage_data
            .fns
            .get(AS_TYPENODE_PROTOCOL_NAME)
            .cloned()
        {
            Some(t) => t,
            None => {
                // If the type-stage scope is non-empty but does not define the
                // `as-typenode` protocol function, this is a protocol violation (D-7):
                // any compliant prelude must provide this function.
                if scope_non_empty {
                    let diag_span = rust_span!();
                    state.diagnostics.push(crate::error::Diagnostic::error(
                        "protocol-violation",
                        format!(
                            "type-stage scope does not define the `{}` protocol function (D-7): \
                             any compliant prelude must export this function for TypeNode dispatch",
                            AS_TYPENODE_PROTOCOL_NAME
                        ),
                        diag_span,
                    ));
                }
                return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags);
            }
        }
    };

    // Step 2: materialise the thunk to get the resolver function value.
    let eval_ctx = match state.eval_ctx.clone() {
        Some(ctx) => ctx,
        None => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
    };
    let (mat_result, mat_diags) = crate::eval::materialize(&resolver_thunk, None, &eval_ctx).await.into_parts();
    _dr_diags.extend(mat_diags);
    let fn_val = match mat_result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    // Step 3: call the resolver with the TypeNode value as its sole argument.
    // Invoke inline rather than via a helper so the result conversion uses
    // typenode_value_to_type ONLY (no as_type_dispatch fallback), preventing infinite
    // recursion if the resolver itself returns an unrecognised tag.
    let origin_span = rust_span!();
    let arg_thunk = Arc::new(Thunk::value(val.clone(), origin_span.clone()));

    let result_thunk = match fn_val {
        Value::Function {
            ref clauses,
            ref definition_scope,
            ..
        } => {
            // Arity check: AS_TYPENODE_PROTOCOL_NAME must be a single-parameter function.
            // All clauses must have exactly one parameter — multi-clause as-typenode functions
            // with different arities are malformed.
            let all_single_param = clauses.iter().all(|c| c.params.len() == 1);
            if !all_single_param || clauses.is_empty() {
                return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags);
            }
            let call_ctx = crate::eval_call::CallContext {
                clauses,
                definition_scope: definition_scope.clone(),
                caller_scope: eval_ctx.current_scope.clone(),
                positional: &[arg_thunk],
                named: None,
                call_span: origin_span.clone(),
                ctx: &eval_ctx,
                type_instantiation: std::collections::HashMap::new(),
            };
            {
                let (inv_result, inv_diags) = crate::eval_call::invoke_function(&call_ctx).await.into_parts();
                _dr_diags.extend(inv_diags);
                match inv_result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            }
        }
        _ => return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags),
    };

    let (mat_result2, mat_diags2) = crate::eval::materialize(&result_thunk, None, &eval_ctx).await.into_parts();
    _dr_diags.extend(mat_diags2);
    let result_val = match mat_result2 {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    // Convert using typenode_value_to_type ONLY — no as_type_dispatch fallback here.
    // This prevents infinite recursion: if the resolver itself returns an unrecognised tag,
    // we return Ok(None) rather than attempting further dispatch.
    // Pass empty type_stage_scope: as_type_dispatch is itself the dispatch path for
    // unrecognised TypeConstructor names; recursing into scope lookup here would loop.
    let (tv_result, tv_diags) = typenode_value_to_type(&result_val, &eval_ctx, &[], &state.type_name_to_id).await.into_parts();
    _dr_diags.extend(tv_diags);
    match tv_result {
        Ok(v) => crate::error::DiagnosticResult::ok(v).with_diagnostics(_dr_diags),
        Err(e) => crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    }
}

/// Evaluate a type-stage `SurfaceNode` annotation expression and convert the result to
/// a `Type`.
///
/// This is the primary entry point for evaluating `@[expr]` annotations that contain
/// user-defined type-stage expressions — such as user-defined type combinators, `mu`
/// expressions, or any other type-stage call that the name-based combinator dispatch in
/// `resolve_type_dict` does not recognize.
///
/// ## Evaluation Pipeline
///
/// ```text
/// SurfaceNode (annotation expr)
///   → lower::lower → CoreExpr
///   → Thunk::core_expr in a minimal EvalContext
///   → materialize(...).await   (produces TypeNode Value)
///   → typenode_value_to_type
///   → Type
/// ```
///
/// ## Error Behaviour
///
/// Returns `Err(Diagnostic)` if:
/// - Evaluation of the expression fails (runtime error in type-stage code).
/// - The evaluated value cannot be converted to a Type (unrecognized TypeNode tag).
///
/// Errors propagate to callers. `resolve_property_dict_as_record` falls back to
/// type-stage evaluation when type-dict resolution fails; on eval failure it returns
/// the eval error as the primary diagnostic with the dict-resolution error as a note.
/// Evaluate a surface expression to a raw Value using the real evaluator pipeline.
/// Lowers the node to CoreExpr, wraps it as a thunk, and materializes it.
/// Handles arbitrary expressions including dot-access chains of any depth.
pub(crate) async fn eval_expr(
    node: &Arc<SurfaceNode>,
    state: &InferState,
) -> crate::error::DiagnosticResult<crate::value::Value> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let node_span = node.span.clone();

    let ctx: std::sync::Arc<crate::eval::EvalContext> = if let Some(ref eval_ctx) = state.eval_ctx {
        std::sync::Arc::clone(eval_ctx)
    } else {
        crate::eval::EvalContext::new_empty()
    };

    let scope: crate::symbol_table::ScopeChain = state
        .eval_scope
        .as_ref()
        .cloned()
        .unwrap_or_else(|| ctx.current_scope.clone());

    let thunk = crate::eval::eval_expr(node, &scope, &ctx)
        .take(&ctx.runtime_diagnostics)
        .expect("eval_expr always succeeds");

    let (mat_result, mat_diags) = crate::eval::materialize(&thunk, Some(&node_span), &ctx).await.into_parts();
    _dr_diags.extend(mat_diags);
    match mat_result {
        Ok(v) => crate::error::DiagnosticResult::ok(v).with_diagnostics(_dr_diags),
        Err(e) => crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    }
}

pub(crate) async fn eval_type_stage_expr(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
) -> crate::error::DiagnosticResult<TypeValue> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let node_span = node.span.clone();
    let (ee_result, ee_diags) = eval_expr(node, state).await.into_parts();
    _dr_diags.extend(ee_diags);
    let typenode_val = match ee_result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    let ctx: std::sync::Arc<crate::eval::EvalContext> = if let Some(ref eval_ctx) = state.eval_ctx {
        std::sync::Arc::clone(eval_ctx)
    } else {
        crate::eval::EvalContext::new_empty()
    };

    // Convert the materialized TypeNode Value to a Type.
    // If the direct structural conversion fails (unrecognised TypeNode tag), try
    // as_type_dispatch to normalise the value through the prelude's `as-typenode`
    // function before giving up.
    // Propagate eval errors from typenode_value_to_type and as_type_dispatch as Diagnostics.
    let (tv_result, tv_diags) = typenode_value_to_type(
        &typenode_val,
        &ctx,
        &state.type_stage_data.scope,
        &state.type_name_to_id,
    )
    .await
    .into_parts();
    _dr_diags.extend(tv_diags);
    match tv_result {
        Ok(Some(ty)) => return crate::error::DiagnosticResult::ok(ty).with_diagnostics(_dr_diags),
        Ok(None) => {}
        Err(e) => {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("type-stage field materialization failed: {e}"),
                node_span.clone(),
            )).with_diagnostics(_dr_diags);
        }
    }
    let (atd_result, atd_diags) = as_type_dispatch(&typenode_val, state).await.into_parts();
    _dr_diags.extend(atd_diags);
    match atd_result {
        Ok(Some(ty)) => return crate::error::DiagnosticResult::ok(ty).with_diagnostics(_dr_diags),
        Ok(None) => {}
        Err(e) => {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("type-stage field materialization failed: {e}"),
                node_span.clone(),
            )).with_diagnostics(_dr_diags);
        }
    }
    crate::error::DiagnosticResult::err(Diagnostic::error(
        KIND,
        format!(
            "type-stage expression produced an unrecognized value: {}",
            typenode_val.render(state.tycon_env_ref())
        ),
        node_span,
    )).with_diagnostics(_dr_diags)
}

// ============================================================================
// Alias Expansion: T-1066 expand_named / T-1067 expand_all_tycon_apps
// ============================================================================

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
async fn try_resolve_fn_type_expr(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
) -> Result<Option<TypeValue>, Diagnostic> {
    let first = match entries.first() {
        Some(e) if e.node.key.is_none() => e,
        _ => return Ok(None),
    };

    // Annotated VarRef: annotation is now on VarRef directly.
    let (ann_node, ann_span) = match &first.node.value.expr {
        SurfaceExpression::VarRef {
            name,
            annotation: Some(annotation),
            ..
        } if name == "Fn" => (&annotation.node, annotation.span.clone()),
        _ => return Ok(None),
    };

    if entries.len() != 2 {
        return Err(Diagnostic::error(
            KIND,
            format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
        ));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(Diagnostic::error(
            KIND,
            "function type parameter list must be auto-indexed",
            second.span.clone(),
        ));
    }

    let ret = resolve_annotation_as_type(
        ann_node,
        ann_span,
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
    )
    .await?;

    // The parameter list can be:
    // - SurfaceExpression::Dict { entries: entries, scope: std::sync::OnceLock::new() } — standard syntax: `[$a $b]` or `[$Number]`
    //   (unnamed params) or new syntax: `[x: String  y: Bool]` (named params)
    // - SurfaceExpression::Call { implied: true, func, args } — bare identifiers like `[a b]`
    //   parse as implied calls. Extract func + args as the parameter type expressions.
    let mut params = Vec::new();
    match &second.node.value.expr {
        SurfaceExpression::Dict {
            entries: param_entries,
            scope: _,
        } => {
            for entry in param_entries.iter() {
                // Extract parameter name from key if present
                let param_name = if let Some(ref key) = entry.node.key {
                    match &key.expr {
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        SurfaceExpression::StringLiteral { content: s, .. } => Some(s.clone()),
                        _ => None,
                    }
                } else {
                    None
                };
                let param_ty = resolve_type_expr(
                    &entry.node.value,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await?;
                params.push((param_name, param_ty, entry.span.clone()));
            }
        }
        SurfaceExpression::Call {
            implied: true,
            func,
            args,
            ..
        } => {
            // New syntax: [TypeA TypeB] parses as an implied call.
            // Treat func as the first param, args as remaining params.
            let param_ty = resolve_type_expr(
                func,
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await?;
            params.push((None, param_ty, func.span.clone()));
            for arg in args.iter() {
                let param_ty = resolve_type_expr(
                    arg,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await?;
                params.push((None, param_ty, arg.span.clone()));
            }
        }
        _ => {
            return Err(Diagnostic::error(
                KIND,
                "function type parameter list must be a bracket expression",
                second.node.value.span.clone(),
            ))
        }
    }

    Ok(Some(make_typevalue_fn(params, ret)))
}
