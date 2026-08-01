//! Type annotation resolution and type expression parsing.
//!
//! After the S-1003 runtime-types migration, all type-level values are represented as
//! `Arc<Value>` (TypeValue) with TypeValue.* constructor tags rather than the `Type` enum.
//! The `Type` enum has been deleted; this file works with TypeValue throughout.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::error::TypeDiagnostic;
use crate::rust_span;
use crate::type_class::TypeValue;
use crate::type_def::TyConDef;
use crate::type_infer::{
    make_rowtail_uniform, make_typevalue_app, make_typevalue_float_lit, make_typevalue_fn,
    make_typevalue_fn_with_flags, make_typevalue_int_lit, make_typevalue_negation,
    make_typevalue_never, make_typevalue_nominal_variant, make_typevalue_op, make_typevalue_record,
    make_typevalue_recursive, make_typevalue_repr, make_typevalue_str_lit, make_typevalue_top,
    make_typevalue_unknown, make_typevalue_var, typevalue_normalize_intersection,
    typevalue_normalize_union,
};
use crate::type_tags::*;
use crate::types::InferState;
// Constraint is now Vec<Arc<Value>> — no Constraint type import needed.
// Kind is deleted — kind names are now plain strings ("Type", "Operator", "Label").
use crate::value::{HashableValue, Thunk, Value};

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
) -> Result<(TypeValue, Option<String>), TypeDiagnostic> {
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
                        SurfaceExpression::Dict(bind_entries) => {
                            for bind_entry in bind_entries {
                                if bind_entry.node.key.is_some() {
                                    return Err(TypeDiagnostic::error("type-error",
                                        "bind: list must contain only positional entries (bare names)".to_string(),
                                        bind_entry.span.clone(),
                                    ));
                                }
                                match &bind_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Check lowercase convention for TypeVar names
                                        if !name.starts_with(|c: char| c.is_lowercase()) {
                                            return Err(TypeDiagnostic::error("type-error",
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
                                            return Err(TypeDiagnostic::error(
                                                "type-error",
                                                "bind: requires an annotation mapping context"
                                                    .to_string(),
                                                span,
                                            ));
                                        }
                                    }
                                    _ => {
                                        return Err(TypeDiagnostic::error(
                                            "type-error",
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
                                return Err(TypeDiagnostic::error(
                                    "type-error",
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
                                            return Err(TypeDiagnostic::error(
                                                "type-error",
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
                                            _ => return Err(TypeDiagnostic::error(
                                                "type-error",
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
                                    return Err(TypeDiagnostic::error("type-error",
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
                                    return Err(TypeDiagnostic::error(
                                        "type-error",
                                        "bind: requires an annotation mapping context",
                                        span,
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(TypeDiagnostic::error(
                                "type-error",
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
                        SurfaceExpression::Dict(kinds_entries) => {
                            for kind_entry in kinds_entries {
                                let typevar_name = match &kind_entry.node.key {
                                    Some(k) => {
                                        match &k.expr {
                                            SurfaceExpression::StringLiteral {
                                                content: s, ..
                                            } => s.clone(),
                                            _ => return Err(TypeDiagnostic::error(
                                                "type-error",
                                                "kinds: keys must be bare words (TypeVar names)",
                                                kind_entry.span.clone(),
                                            )),
                                        }
                                    }
                                    None => {
                                        return Err(TypeDiagnostic::error(
                                            "type-error",
                                            "kinds: entries must be keyed [name: kind]".to_string(),
                                            kind_entry.span.clone(),
                                        ))
                                    }
                                };

                                // Validate that this name was declared in bind:
                                if let Some(ref mapping) = ann_mapping {
                                    if !mapping.contains_key(&typevar_name) {
                                        return Err(TypeDiagnostic::error(
                                            "type-error",
                                            format!(
                                                "kinds: TypeVar '{}' not found in bind: list",
                                                typevar_name
                                            ),
                                            kind_entry.span.clone(),
                                        ));
                                    }
                                } else {
                                    return Err(TypeDiagnostic::error(
                                        "type-error",
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
                                        return Err(TypeDiagnostic::error(
                                            "type-error",
                                            format!(
                                                "unknown kind '{}' (valid: Operator, Label)",
                                                kind_name
                                            ),
                                            kind_entry.node.value.span.clone(),
                                        ));
                                    }
                                    _ => {
                                        return Err(TypeDiagnostic::error(
                                            "type-error",
                                            "kinds: value must be a kind name (Operator or Label)"
                                                .to_string(),
                                            kind_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeDiagnostic::error(
                                "type-error",
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
                        SurfaceExpression::Dict(constraint_entries) => {
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
                                            return Err(TypeDiagnostic::error(
                                                "type-error",
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
                                    return Err(TypeDiagnostic::error("type-error", "constraint annotations require an annotation mapping context".to_string(),
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
                                            return Err(TypeDiagnostic::error("type-error",
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
                                                                return Err(TypeDiagnostic::error("type-error",
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
                                                    return Err(TypeDiagnostic::error("type-error",
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
                                        return Err(TypeDiagnostic::error("type-error",
                                            "constraint value must be a class name or list of class names".to_string(),
                                            c_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeDiagnostic::error(
                                "type-error",
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
                        SurfaceExpression::Dict(constraint_entries) => {
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

                                        // Validate the class exists in env and get the ClassDecl.
                                        // During bootstrap (e.g. test-loader.llt type-checked
                                        // before prelude loads), classes like Indexable/Indexed
                                        // may not exist yet. Skip the constraint group rather
                                        // than erroring — gradual typing: unresolvable constraints
                                        // in a context without the class definition are ignored.
                                        let class_decl_opt = {
                                            let env_guard = state.env.read().unwrap();
                                            env_guard.get_class(class_name)
                                        };
                                        let class_decl = if let Some(cd) = class_decl_opt {
                                            cd
                                        } else {
                                            // Skip this MPTC group: advance past the class name
                                            // and any subsequent positional TypeVar entries.
                                            i += 1;
                                            while i < constraint_entries.len() {
                                                let subsequent = &constraint_entries[i];
                                                if subsequent.node.key.is_some() {
                                                    break;
                                                }
                                                match &subsequent.node.value.expr {
                                                    SurfaceExpression::VarRef {
                                                        escaped: true,
                                                        ..
                                                    } => break,
                                                    _ => {}
                                                }
                                                i += 1;
                                            }
                                            continue;
                                        };

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
                                                            return Err(TypeDiagnostic::error("type-error",
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
                                                        return Err(TypeDiagnostic::error("type-error",
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
                                                    return Err(TypeDiagnostic::error("type-error",
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
                                        return Err(TypeDiagnostic::error("type-error",
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
                            return Err(TypeDiagnostic::error(
                                "type-error",
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
                    state.diagnostics.push(crate::error::TypeDiagnostic::warn(
                        "unknown-type-param",
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
) -> Result<TypeValue, TypeDiagnostic> {
    match ann {
        Annotation::PropertyDict(surface_entries) => {
            // Dispatch: if any entry has a named key matching function metadata keys
            // (return:, constraint:, doc:, bind:, kinds:), treat as fn metadata dict.
            // If all entries are positional, delegate to resolve_type_dict (handles
            // [Fn@Return [Params]] and union-style type expressions).
            let has_fn_key = surface_entries.iter().any(|e| {
                if let Some(ref key) = e.node.key {
                    matches!(&key.expr,
                        SurfaceExpression::StringLiteral { content: s, .. }
                            if crate::ast::STANDARD_ANN_KEYS.contains(&s.as_str()))
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
                            return Err(TypeDiagnostic::error(
                                "invalid-annotation",
                                "property dict annotation requires string (bare-word) keys",
                                k.span.clone(),
                            ));
                        }
                    }
                }
            }

            if has_fn_key || all_keyed {
                // If any entry has a standard key, or if ALL entries are keyed (custom
                // annotation keys like `[cache: true]`), treat as fn metadata dict.
                // fn@[only-custom-keys: val] is correctly handled here: when all entries
                // are keyed (no positional entries), treat as fn metadata dict even without
                // a standard key (has_fn_key false), because all_keyed triggers this path.
                if !all_keyed {
                    return Err(TypeDiagnostic::error("type-error",
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
        _ => {
            // Simple(name) path: fn@Int, fn@a, etc.
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
) -> Result<TypeValue, TypeDiagnostic> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
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
        Annotation::Quote => {
            // The quoting annotation constrains the value to be an AST node — a
            // runtime-determined type. The type checker cannot statically determine
            // which concrete type is produced, so produce a fresh type variable
            // (consistent with typecheck_narrow.rs:368).
            Ok(state.fresh_type_var(&span))
        }
        Annotation::PropertyDict(surface_entries) => {
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
        Annotation::Annotated(outer, inner) => {
            // For fn annotations, forward to full resolver
            // (e.g., fn@Seq@Int should resolve the Annotated properly)
            resolve_annotation(
                &Annotation::Annotated(outer.clone(), inner.clone()),
                span,
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await
        }
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
) -> Result<TypeValue, TypeDiagnostic> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
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
        Annotation::Quote => {
            // The quoting annotation constrains the value to be an AST node — a
            // runtime-determined type. The type checker cannot statically determine
            // which concrete type is produced, so produce a fresh type variable
            // (consistent with typecheck_narrow.rs:368).
            Ok(state.fresh_type_var(&span))
        }
        Annotation::Annotated(outer, inner) => {
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
            // Extract the name from outer for resolve_type_head dispatch.
            // When outer is a Simple annotation (the common case: Seq@Int, Map@[K:V]),
            // call resolve_type_head directly. For non-Simple outers (PropertyDict@Inner),
            // resolve the outer to a type and produce App(outer_ty, arg).
            match outer.as_ref() {
                Annotation::Simple(name) => {
                    Box::pin(resolve_type_head(name, &[arg], state, constraints, span)).await
                }
                _ => {
                    // Non-Simple outer: resolve outer as a type then apply arg to it.
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
        Annotation::PropertyDict(surface_entries) => {
            // PropertyDict can mean different things depending on its keys:
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
                    SurfaceExpression::StringLiteral { .. } => Err(TypeDiagnostic::error(
                        "type-error",
                        "label: value must be a bare name (e.g. `label: l`), not a string literal",
                        span,
                    )),
                    SurfaceExpression::VarRef { name, .. } => {
                        if name.starts_with(|c: char| c.is_uppercase()) {
                            Err(TypeDiagnostic::error("type-error",
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
                            debug_assert!(
                                state.get_level(&fresh).is_some(),
                                "invariant: label var just inserted must be present in state.ctx.levels"
                            );
                            Ok(make_typevalue_var(&fresh))
                        }
                    }
                    _ => Err(TypeDiagnostic::error(
                        "type-error",
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
                                return Err(TypeDiagnostic::error(
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
) -> Result<TypeValue, TypeDiagnostic> {
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
                        if state.tycon_env.contains_key(name.as_str()))
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
                match eval_type_stage_expr(&synth_node, state).await {
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
                | SurfaceExpression::Dict(_)
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
            ctor,
            payload: Some(t),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => t,
        _ => return None,
    };
    let dict = match payload_thunk.peek_result()? {
        Ok(Value::Dict { entries, .. }) => entries,
        _ => return None,
    };
    let class_thunk = dict.get(&HashableValue::Str(Arc::from(FIELD_CLASS)))?;
    match class_thunk.peek_result()? {
        Ok(Value::Variant {
            ctor: c_ctor,
            payload: Some(inner_thunk),
            ..
        }) if c_ctor.as_ref() == TV_OP => match inner_thunk.peek_result()? {
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
            ctor,
            payload: Some(t),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => t,
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
) -> Result<TypeValue, TypeDiagnostic> {
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
    //      that an instance exists. If no instance is found, emit a TypeDiagnostic::error.
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
            let env_guard = state.env.read().unwrap();
            let class_opt = env_guard.get_class(&class_name);
            match class_opt {
                None => true, // Unknown class — not yet in env (e.g., forward reference); defer checking.
                Some(class_decl) => {
                    if class_decl.method_signatures.is_empty() {
                        // No method signatures registered yet (class body not yet processed).
                        // Defer constraint checking — assume satisfied to avoid false errors.
                        true
                    } else {
                        let tags_ref: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
                        class_decl.method_signatures.iter().any(|(method_name, _)| {
                            let mangled = crate::type_def::instance_binding_name(
                                &class_name,
                                method_name,
                                &tags_ref,
                            );
                            env_guard.get_instance(&mangled).is_some()
                        })
                    }
                }
            }
        };
        if !instance_found {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!(
                    "type argument(s) {:?} do not satisfy constraint '{}' required by type alias",
                    tags, class_name,
                ),
                rust_span!(),
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
///   1. `state.type_stage_scope` (resolved TypeValues): return directly (with App wrapping if args present)
///   2. `state.type_stage_fns` (Function thunks): call the thunk with args
///   3. `state.type_stage_type_vars` (TypeVar kinds): fresh TypeVar of the given kind
///   4. `state.env` (ClassDecl): fresh TypeVar with a class constraint
///   5. Undefined → TypeDiagnostic
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
) -> Result<TypeValue, TypeDiagnostic> {
    // Step 1: check type_stage_scope for a resolved TypeValue.
    for scope in &state.type_stage_scope {
        if let Some(tv) = scope.get(name) {
            // Fully materialized TypeValue (e.g., TypeNode.Int → TypeValue.Repr) — return directly.
            if args.is_empty() {
                return Ok(Arc::clone(tv));
            }
            let mut result = Arc::clone(tv);
            for arg in args {
                result = make_typevalue_app(result, Arc::clone(arg));
            }
            return Ok(result);
        }
    }

    // Step 2: check type_stage_fns for a parameterized type constructor thunk.
    if let Some(thunk) = state.type_stage_fns.get(name).cloned() {
        // Function thunk — parameterized type constructor (e.g., Seq, Result).
        if args.is_empty() {
            // Zero-arg reference to a parameterized type constructor (e.g.,
            // @[is: Seq] or @[narrows: Seq]). The caller wants the unapplied
            // type constructor — produce TypeValue.Op(name) which represents "any
            // application of this constructor" (Seq of any element type).
            // This enables annotation-based narrowing for parameterized types
            // without requiring a type argument.
            return Ok(make_typevalue_op(name));
        }
        if let Some(eval_ctx) = &state.eval_ctx {
            match crate::type_normalize::evaluate_resolver_with_thunk(thunk, args, eval_ctx).await {
                Ok(Some(ty)) => return Ok(ty),
                Ok(None) => {
                    // Resolver value not applicable — fall through.
                }
                Err(eval_err) => {
                    return Err(TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "type constructor `{}` failed during evaluation: {}",
                            name, eval_err
                        ),
                        span,
                    ));
                }
            }
        } else {
            // eval_ctx unavailable — cannot invoke a type-stage function without
            // an evaluation context. This is a misconfiguration error, not a
            // lookup failure; emit a diagnostic rather than silently falling through
            // to "undefined type: X" which would hide the real problem.
            return Err(TypeDiagnostic::error(
                "type-error",
                format!(
                    "type constructor `{}` requires an evaluation context (eval_ctx is None)",
                    name
                ),
                span,
            ));
        }
    }

    // Step 3: check type_stage_type_vars for a TypeVar kind annotation.
    if let Some(kind) = state.type_stage_type_vars.get(name).cloned() {
        // Operator-kinded names in application position produce App chains.
        // This handles `[m SomeType]` where `m@Operator` was declared in a class.
        let kind_is_operator = kind == "Operator";
        if kind_is_operator && !args.is_empty() {
            if args.len() != 1 {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "type constructor `{name}` requires 1 type argument, got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let a_type = Arc::clone(&args[0]);
            // Check if a_type is a TypeValue.Op (kind-mismatch: cannot apply Op to Op)
            if crate::type_infer::typevalue_is_op(&a_type) {
                let op_name = crate::type_infer::typevalue_op_name(&a_type)
                    .unwrap_or_else(|| "<unknown>".to_string());
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "kind mismatch: type constructor `{name}` cannot be \
                         applied to another type constructor `{op_name}`; \
                         use a concrete type instead"
                    ),
                    span,
                ));
            }
            return Ok(make_typevalue_app(make_typevalue_op(name), a_type));
        }
        let level = state.ctx.current_level;
        let (fresh, _fresh_ty) =
            state.fresh_type_var_with(Some(name), Some(level), kind.as_str(), &span);
        return Ok(make_typevalue_var(&fresh));
    }

    // Step 4: check env (class registry) for a class declaration.
    let class_decl_opt = {
        let env_read = state.env.read().unwrap();
        env_read.get_class(name).map(|c| c.clone())
    };
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

    // Not found in any scope — undefined type name.
    Err(TypeDiagnostic::error(
        "undefined-type",
        format!("undefined type: {}", name),
        span,
    ))
}

pub(crate) async fn resolve_type_name(
    name: &str,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, TypeValue>, bool)>,
    row_ann_mapping: &Option<&HashMap<String, String>>,
) -> Result<TypeValue, TypeDiagnostic> {
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
                    // either find a bind:-declared TypeVar or produce a TypeDiagnostic.
                    let in_params = ann_mapping.as_ref().is_some_and(|m| m.contains_key(name));
                    if strict && !in_params && !params.contains_key(name) {
                        // Name not declared as a type parameter — check if it's a scope reference.
                        if !state.tycon_env.contains_key(name) {
                            return Err(TypeDiagnostic::error(
                                "type-error",
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
                    return Err(TypeDiagnostic::error("type-error",
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
                        debug_assert!(
                            state.get_level(existing_var).is_some(),
                            "invariant: annotation var registered in mapping must be in state.ctx.levels"
                        );
                        Ok(make_typevalue_var(existing_var))
                    } else {
                        Err(TypeDiagnostic::error(
                            "undefined-type",
                            format!("undefined type: {name}"),
                            span,
                        ))
                    }
                } else {
                    Err(TypeDiagnostic::error(
                        "undefined-type",
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
) -> Result<TypeValue, TypeDiagnostic> {
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
                // reconstruct the `Annotation::Annotated(name, inner)` and dispatch through
                // `resolve_annotation` which handles `"Handle"`, `"Seq"`, `"Map"`, etc.
                let full_ann = Annotation::Annotated(
                    Box::new(Annotation::Simple(name.clone())),
                    Box::new(annotation.node.clone()),
                );
                Box::pin(resolve_annotation(
                    &full_ann,
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
                    // Recover only for "undefined-type" errors on constructor-shaped names
                    // (uppercase initial character). These arise when a bare uppercase name
                    // like `None` or `Red` is used in annotation position before the type
                    // declaration is registered — the name is a unit nominal variant.
                    // Propagate all other errors (e.g., kind mismatches, internal errors).
                    if e.kind == "undefined-type" {
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
        SurfaceExpression::Dict(entries) => {
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
                            SurfaceExpression::Dict(param_entries) => {
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
                                    params.push((param_name, param_ty));
                                }
                            }
                            SurfaceExpression::Call {
                                implied: true,
                                func: inner_func,
                                args: inner_args,
                                ..
                            } => {
                                // Param list itself is an implied call: [a b c] → VarRef("a") + args
                                let param_ty = Box::pin(resolve_type_expr(
                                    inner_func,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                params.push((None, param_ty));
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
                                    params.push((None, param_ty));
                                }
                            }
                            _ => {
                                // Single param that's not a Dict
                                let param_ty = Box::pin(resolve_type_expr(
                                    param_list,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                params.push((None, param_ty));
                            }
                        }
                    }
                    if args.len() > 1 {
                        return Err(TypeDiagnostic::error("type-error",
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

            // TyConDef-based type constructor application (T-949) in implied-call position.
            // Primary path for user-defined type constructors in [TyCon Arg1 Arg2 ...] form.
            // Primary path: look up via TyConDef (covers user-defined types and builtin TyCons
            // registered in T-1018: Seq, Map, Handle). Falls through to kind_env for unregistered names.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if let Some(def) = state.tycon_env.get(name).cloned() {
                    let arity = def.arity();
                    if arity > 0 {
                        let mut result = make_typevalue_op(name);
                        let arg_count = std::cmp::min(arity, args.len());
                        for arg_node in args.iter().take(arg_count) {
                            let arg = Box::pin(resolve_type_expr(
                                arg_node,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            result = make_typevalue_app(result, arg);
                        }
                        return Ok(result);
                    }
                }
            }

            // Check if this is a parameterized type alias application: [AliasName Arg1 Arg2]
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if let Some(alias) = state.tycon_env.get(name).cloned() {
                    // Resolve all type arguments
                    let mut type_args = Vec::new();
                    for arg in args {
                        type_args.push(
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

                    // Check arity
                    if type_args.len() != alias.params.len() {
                        return Err(TypeDiagnostic::error(
                            "type-error",
                            format!(
                                "type alias '{}' expects {} type parameter(s), got {}",
                                name,
                                alias.params.len(),
                                type_args.len()
                            ),
                            node.span.clone(),
                        ));
                    }

                    // Build substitution and apply to body
                    return Box::pin(instantiate_tycon_def(alias.as_ref(), &type_args, state))
                        .await;
                }
            }

            // Nominal constructor: [ConstructorName field1: T1 field2: T2 ...]
            // Check if func is an uppercase VarRef (nominal constructor name).
            // Builtin type names (Int, Float, etc.) must NOT be treated as NominalVariant.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                let tycon_env_ref = state.tycon_env_ref();
                let is_builtin = tycon_env_ref
                    .get(name)
                    .is_some_and(|def| def.builtin_type.is_some());
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
                            return Ok(typevalue_normalize_union(members));
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
                    return eval_type_stage_expr(node, state).await;
                }
            }

            Err(TypeDiagnostic::error(
                "type-error",
                format!("invalid type expression in annotation: {:?}", node.expr),
                node.span.clone(),
            ))
        }
        _ => Err(TypeDiagnostic::error(
            "type-error",
            format!("invalid type expression in annotation: {:?}", node.expr),
            node.span.clone(),
        )),
    }
}

/// Reverse-lookup: given a bare constructor name (e.g. `"Red"`), find the tycon name
/// (e.g. `"Color"`) by scanning `state.tycon_env` for any tycon whose `constructors` vec
/// contains an entry whose qualified tag (`"Color.Red"`) ends with `".CtorName"`.
///
/// Returns the tycon name (e.g. `"Color"`) if found, or an empty string if not.
/// Used by `resolve_type_expr` to populate `NominalVariant.tycon` when only the ctor
/// name is available (e.g. bare uppercase VarRef fallback, implied-call constructor).
fn lookup_tycon_for_ctor(state: &InferState, ctor_name: &str) -> String {
    for (tycon_name, def) in &state.tycon_env {
        for (qualified_tag, _arity) in &def.constructors {
            // Qualified tag format: "TyConName.CtorName"
            if let Some((_, bare_ctor)) = qualified_tag.split_once('.') {
                if bare_ctor == ctor_name {
                    return tycon_name.clone();
                }
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
) -> Result<TypeValue, TypeDiagnostic> {
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
                                return Ok(typevalue_normalize_union(members));
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
                        .type_stage_scope
                        .iter()
                        .any(|s| s.contains_key(name.as_str()))
                        || state.type_stage_fns.contains_key(name.as_str())
                        || state.type_stage_type_vars.contains_key(name.as_str())
                        || state.env.read().unwrap().get_class(name.as_str()).is_some();

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
                                return Err(TypeDiagnostic::error(
                                    "type-error",
                                    "named constructor key must be a bare uppercase word",
                                    key_node.span.clone(),
                                ))
                            }
                        };
                        if !crate::eval::is_constructor_name(&ctor_name) {
                            return Err(TypeDiagnostic::error("type-error",
                                format!(
                                    "constructor name must start with an uppercase letter, got `{ctor_name}`"
                                ),
                                key_node.span.clone(),
                            ));
                        }
                        // Resolve the payload dict. The value should be a Dict of named fields.
                        let variant_fields: indexmap::IndexMap<String, Arc<crate::value::Value>> =
                            match &entry.node.value.expr {
                                SurfaceExpression::Dict(field_entries) => {
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
                                                    return Err(TypeDiagnostic::error("type-error",
                                                        "payload field names must be bare words",
                                                        k.span.clone(),
                                                    ))
                                                }
                                            },
                                            None => {
                                                return Err(TypeDiagnostic::error("type-error",
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
                                return Err(TypeDiagnostic::error("type-error",
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
                typevalue_normalize_union(members)
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
                    let tycon_env_ref2 = state.tycon_env_ref();
                    let is_builtin_type = tycon_env_ref2
                        .get(&tag)
                        .is_some_and(|def| def.builtin_type.is_some());
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
                            // "undefined-type" means the name is not a known type in scope;
                            // fall through to the NominalVariant check below.
                            Err(e) if e.kind == "undefined-type" => {}
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
                                            _ => return Err(TypeDiagnostic::error(
                                                "type-error",
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
                                        return Err(TypeDiagnostic::error("type-error",
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
            return Ok(typevalue_normalize_union(members));
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
                return Err(TypeDiagnostic::error("type-error",
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
                    return Err(TypeDiagnostic::error(
                        "type-error",
                        "type record keys must be bare words",
                        k.span.clone(),
                    ))
                }
            },
            None => {
                return Err(TypeDiagnostic::error(
                    "type-error",
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

    // B-662: multi-field record annotations always build a single TypeValue.Record.
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
    // When a uniform tail is present, wrap it as RowTail.Uniform with the value type.
    // The key type is validated above but not yet part of the RowTail.Uniform spec.
    let tail = match uniform_tail {
        Some((_key_ty, val_ty)) => Some(make_rowtail_uniform(val_ty)),
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
///   synthesize `SurfaceExpression::Dict(entries)`. Dict evaluation in the type-stage
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
            SurfaceExpression::Dict(entries.to_vec()),
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
) -> crate::error::EvalResult<Option<HashMap<String, Value>>> {
    let payload_id = match val {
        Value::Variant {
            payload: Some(id), ..
        } => id.clone(),
        _ => return Ok(None),
    };
    let payload_thunk = payload_id;
    let payload_val = crate::eval::materialize(&payload_thunk, None, ctx).await?;
    match payload_val {
        Value::Dict { entries: dict, .. } => {
            // Extract each string-keyed field, materializing the field value.
            let mut fields = HashMap::new();
            for (key, thunk) in &dict {
                if let HashableValue::Str(k) = key {
                    let v = crate::eval::materialize(thunk, None, ctx).await?;
                    fields.insert(k.to_string(), v);
                }
            }
            Ok(Some(fields))
        }
        _ => Ok(None),
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
) -> crate::error::EvalResult<Option<Vec<TypeValue>>> {
    // T-1555: Value::Annotated is removed; no unwrapping needed.
    let dict = match dict_val {
        Value::Dict { entries: d, .. } => d,
        _ => return Ok(None),
    };

    let mut result = Vec::new();
    let mut i = 0i64;
    loop {
        match dict.get(&HashableValue::Int(i)) {
            Some(thunk) => {
                let val = crate::eval::materialize(thunk, None, ctx).await?;
                match Box::pin(typenode_value_to_type(&val, ctx, type_stage_scope)).await? {
                    Some(ty) => {
                        result.push(ty);
                        i += 1;
                    }
                    None => return Ok(None),
                }
            }
            None => return Ok(Some(result)),
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
) -> crate::error::EvalResult<Option<TypeValue>> {
    Box::pin(async move {
        match val {
            // TypeValue.* variants — already in TypeValue form: return as-is.
            // After S-1003, TypeValues (IntLit, StrLit, FloatLit, Record, Repr, etc.)
            // ARE their own TypeNode representation. The roundtrip is identity.
            Value::Variant { ctor, .. }
                if crate::value::tycon_name_from_ctor(ctor.as_ref()) == TYCON_TYPEVALUE =>
            {
                Ok(Some(Arc::new(val.clone())))
            }

            Value::Variant { ctor, .. }
                if crate::value::tycon_name_from_ctor(ctor.as_ref()) == TYCON_TYPENODE =>
            {
                // NOTE: ctor holds the FULLY QUALIFIED constructor name (e.g., "TypeNode.Unknown").
                // Strip the "TypeNode." prefix to get the bare constructor name for dispatch.
                let bare_ctor = ctor
                    .as_ref()
                    .split_once('.')
                    .map(|(_, c)| c)
                    .unwrap_or(ctor.as_ref());
                match bare_ctor {
                    TN_BARE_UNKNOWN => Ok(Some(make_typevalue_unknown())),
                    // Top is the sound lattice top (τ <: Top for all τ).
                    TN_BARE_TOP => Ok(Some(make_typevalue_top())),
                    TN_BARE_NEVER => Ok(Some(make_typevalue_never())),
                    // Absent: the empty closed dict type — the Rust-protocol encoding of the absent/null sentinel.
                    // The empty dict `[]` is Value::Dict({}) at runtime; @Absent is its static type.
                    TN_BARE_ABSENT => {
                        Ok(Some(make_typevalue_record(indexmap::IndexMap::new(), None)))
                    }
                    // Primitive leaf types — no payload, direct mapping to TypeValue.Repr variants.
                    // These appear as field values in TypeNode.Dict payloads (e.g., the
                    // "fields" dict of a Dict TypeNode) and as union/intersection members.
                    TN_BARE_INT => Ok(Some(make_typevalue_repr(REPR_INT))),
                    TN_BARE_FLOAT => Ok(Some(make_typevalue_repr(REPR_FLOAT))),
                    TN_BARE_STRING => Ok(Some(make_typevalue_repr(REPR_STRING))),
                    TN_BARE_BYTES => Ok(Some(make_typevalue_repr(REPR_BYTES))),
                    TN_BARE_PROXY => Ok(Some(make_typevalue_repr(REPR_PROXY))),
                    // Any callable — variadic function with zero required fixed params and unknown
                    // return type. Variadic=true means any number of arguments is accepted; the
                    // arity check skips the "too many arguments" path for variadic functions.
                    // This is DISTINCT from TypeValue.Unknown: Callable asserts the value IS a
                    // function; Unknown means we know nothing about the value at all.
                    TN_BARE_CALLABLE => Ok(Some(make_typevalue_fn_with_flags(
                        vec![],
                        make_typevalue_unknown(),
                        true, // variadic — accepts any number of arguments
                        Vec::new(),
                    ))),

                    // ── Union ─────────────────────────────────────────────────────────────
                    // TypeNode.Union { types: [Seq TypeNode] } → Type::normalize_union(members)
                    TN_BARE_UNION => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let types_val = match fields.get(TN_FIELD_TYPES) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let members =
                            match collect_typenode_seq(types_val, ctx, type_stage_scope).await? {
                                Some(v) => v,
                                None => return Ok(None),
                            };
                        if members.is_empty() {
                            return Ok(None); // Empty union is ill-formed — fall back to Unknown.
                        }
                        Ok(Some(typevalue_normalize_union(members)))
                    }

                    // ── Intersect ────────────────────────────────────────────────────────
                    // TypeNode.Intersect { types: [Seq TypeNode] } → typevalue_normalize_intersection(members)
                    TN_BARE_INTERSECT => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let types_val = match fields.get(TN_FIELD_TYPES) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let members =
                            match collect_typenode_seq(types_val, ctx, type_stage_scope).await? {
                                Some(v) => v,
                                None => return Ok(None),
                            };
                        if members.is_empty() {
                            return Ok(None); // Empty intersection is ill-formed — fall back to Unknown.
                        }
                        Ok(Some(typevalue_normalize_intersection(members)))
                    }

                    // ── Negation ─────────────────────────────────────────────────────────
                    // TypeNode.Negation { inner: TypeNode } → make_typevalue_negation(inner)
                    TN_BARE_NEGATION => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let inner_val = match fields.get(TN_FIELD_INNER) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let inner_type = match typenode_value_to_type(
                            &inner_val,
                            ctx,
                            type_stage_scope,
                        )
                        .await?
                        {
                            Some(t) => t,
                            None => return Ok(None),
                        };
                        Ok(Some(make_typevalue_negation(inner_type)))
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
                    TN_BARE_DICT => {
                        let payload_fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let fields_val = match payload_fields.get(TN_FIELD_FIELDS) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let open_val = match payload_fields.get(TN_FIELD_OPEN) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
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
                                        let v = crate::eval::materialize(thunk, None, ctx).await?;
                                        let ty =
                                            match typenode_value_to_type(&v, ctx, type_stage_scope)
                                                .await?
                                            {
                                                Some(t) => t,
                                                None => return Ok(None),
                                            };
                                        out.insert(k.to_string(), ty);
                                    }
                                }
                                out
                            }
                            // fields is not a Dict — TypeNode.Dict payload is malformed; cannot convert.
                            _ => return Ok(None),
                        };

                        // Optional key-type: and value-type: fields enable typed-key map encoding.
                        // These are the protocol-defined field names per builtin_core.llt:33.
                        // If key-type: is present, use value-type (defaulting to Top) as the
                        // RowTail.Uniform value type. key-type is not yet part of the
                        // RowTail.Uniform spec — it is accepted syntactically but not propagated
                        // into the type system until typed-key map support is implemented (T-2091).
                        let tail = if payload_fields.contains_key(TN_FIELD_KEY_TYPE) {
                            // key-type: present — use value-type (defaulting to Top) for the tail.
                            // value-type: defaults to Top when absent.
                            let value_ty = if let Some(vt_val) =
                                payload_fields.get(TN_FIELD_VALUE_TYPE).cloned()
                            {
                                match typenode_value_to_type(&vt_val, ctx, type_stage_scope).await?
                                {
                                    Some(t) => t,
                                    None => return Ok(None),
                                }
                            } else {
                                make_typevalue_top()
                            };
                            Some(make_rowtail_uniform(value_ty))
                        } else if matches!(&open_val, Value::Int { n, .. } if *n != 0) {
                            // Open record: any field value is allowed — RowTail.Uniform with Top.
                            Some(make_rowtail_uniform(make_typevalue_top()))
                        } else {
                            // Closed record
                            None
                        };

                        Ok(Some(make_typevalue_record(record_fields, tail)))
                    }

                    // ── Arrow ─────────────────────────────────────────────────────────────
                    // TypeNode.Arrow { params: [Seq TypeNode], result: TypeNode }
                    // → Type::Function { params: Vec<(None, Type)>, ret: Box<Type>, variadic: false }
                    TN_BARE_ARROW => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let params_val = match fields.get(TN_FIELD_PARAMS) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let result_val = match fields.get(TN_FIELD_RESULT) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };

                        let param_types =
                            match collect_typenode_seq(params_val, ctx, type_stage_scope).await? {
                                Some(v) => v,
                                None => return Ok(None),
                            };
                        let ret_type =
                            match typenode_value_to_type(&result_val, ctx, type_stage_scope).await?
                            {
                                Some(t) => t,
                                None => return Ok(None),
                            };

                        let param_types_named: Vec<(Option<String>, TypeValue)> =
                            param_types.into_iter().map(|ty| (None, ty)).collect();
                        Ok(Some(make_typevalue_fn(param_types_named, ret_type)))
                    }

                    // ── TypeConstructor ───────────────────────────────────────────────────
                    // TypeNode.TypeConstructor { name: String }
                    // Bare (transient): name without '.' → Type::TyCon(name) for expansion
                    // Qualified (leaf): name with '.' → Type::NominalVariant or TyCon leaf
                    TN_BARE_TYPE_CONSTRUCTOR => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return Ok(None),
                        };
                        // Look up the name in the type-stage scope chain.
                        // The type-stage scope holds resolved TypeValues directly.
                        // This covers primitive aliases (Integer, Float, String, etc.) and any
                        // user-defined or prelude-defined type-stage names.
                        for scope in type_stage_scope {
                            if let Some(tv) = scope.get(&name) {
                                return Ok(Some(tv.clone()));
                            }
                        }
                        // Not found in the type-stage scope — treat as an opaque TyCon.
                        // This is correct for types declared in the runtime section (e.g., Color,
                        // BuilderHandle) that are not yet in the type-stage scope chain.
                        Ok(Some(make_typevalue_op(&name)))
                    }

                    // ── TypeApplication ───────────────────────────────────────────────────
                    // TypeNode.TypeApplication { ctor: TypeNode, args: [Seq TypeNode] }
                    // → left-associative Type::App chain: App(App(ctor, args[0]), args[1])...
                    TN_BARE_TYPE_APPLICATION => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let ctor_val = match fields.get(TN_FIELD_CTOR) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let args_val = match fields.get(TN_FIELD_ARGS) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };

                        let ctor_type =
                            match typenode_value_to_type(&ctor_val, ctx, type_stage_scope).await? {
                                Some(t) => t,
                                None => return Ok(None),
                            };
                        let arg_types =
                            match collect_typenode_seq(args_val, ctx, type_stage_scope).await? {
                                Some(v) => v,
                                None => return Ok(None),
                            };

                        if arg_types.is_empty() {
                            // Zero-arg application — return the constructor itself.
                            return Ok(Some(ctor_type));
                        }

                        // Build left-associative App chain.
                        let mut result = ctor_type;
                        for arg in arg_types {
                            result = make_typevalue_app(result, arg);
                        }
                        Ok(Some(result))
                    }

                    // ── TypeVar ───────────────────────────────────────────────────────────
                    // TypeNode.TypeVar { name: String, kind: String }
                    // → TypeValue.Var { name }  (level lives in InferenceContext, not in TypeValue)
                    TN_BARE_TYPE_VAR => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return Ok(None),
                        };
                        // Level is not stored in TypeValue.Var — it lives in InferenceContext.
                        Ok(Some(make_typevalue_var(&name)))
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
                    TN_BARE_RECURSIVE => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        // The "var" field names the binder but is not needed for de Bruijn
                        // representation — only its presence is required for a well-formed
                        // TypeNode.Recursive payload.
                        if !fields.contains_key(TN_FIELD_VAR) {
                            return Ok(None);
                        }
                        let body_val = match fields.get(TN_FIELD_BODY) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let body = match Box::pin(typenode_value_to_type(
                            &body_val,
                            ctx,
                            type_stage_scope,
                        ))
                        .await?
                        {
                            Some(t) => t,
                            None => return Ok(None),
                        };
                        Ok(Some(make_typevalue_recursive(body)))
                    }

                    // ── RecursiveRef ──────────────────────────────────────────────────────
                    // TypeNode.RecursiveRef { name: String }
                    // → TypeValue.Var { name }  (de Bruijn back-reference at depth 0)
                    //
                    // RecursiveRef is a leaf node marking a back-reference to the enclosing
                    // TypeNode.Recursive binder. In the TypeValue representation, this is
                    // TypeValue.Var with the same binder name — the InferenceContext tracks
                    // that this var is a recursive self-reference (never generalized).
                    TN_BARE_RECURSIVE_REF => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let name_val = match fields.get(TN_FIELD_NAME) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        let name = match name_val.as_str() {
                            Some(s) => s.to_string(),
                            None => return Ok(None),
                        };
                        // RecursiveRef maps to TypeValue.Var — same name as the μ-binder.
                        // The InferenceContext knows this var is a recursive self-reference.
                        Ok(Some(make_typevalue_var(&name)))
                    }

                    // ── IntLiteral ────────────────────────────────────────────────────────
                    // TypeNode.IntLiteral { n: Int } → Type::IntLiteral(n)
                    // Produced by union/or when a raw integer value is normalized via
                    // _wrap-typenode-value in the type-stage prelude (e.g. `[or 0 1]`).
                    TN_BARE_INT_LITERAL => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let n_val = match fields.get(TN_FIELD_N) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        match n_val {
                            Value::Int { n, .. } => Ok(Some(make_typevalue_int_lit(n))),
                            _ => Ok(None),
                        }
                    }

                    // ── FloatLiteral ──────────────────────────────────────────────────────
                    // TypeNode.FloatLiteral { value: Float } → TypeValue.FloatLit
                    TN_BARE_FLOAT_LIT => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let v_val = match fields.get(TN_FIELD_FLOAT_VALUE) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        match v_val {
                            Value::Float { n, .. } => Ok(Some(make_typevalue_float_lit(n))),
                            _ => Ok(None),
                        }
                    }

                    // ── StringLiteral ─────────────────────────────────────────────────────
                    // TypeNode.StringLiteral { s: String } → TypeValue.StrLit
                    TN_BARE_STRING_LITERAL => {
                        let fields = match variant_payload_dict(val, ctx).await? {
                            Some(f) => f,
                            None => return Ok(None),
                        };
                        let s_val = match fields.get(TN_FIELD_S) {
                            Some(v) => v.clone(),
                            None => return Ok(None),
                        };
                        match s_val.as_str() {
                            Some(s) => Ok(Some(make_typevalue_str_lit(s))),
                            None => Ok(None),
                        }
                    }

                    // Unknown tag — not a recognized TypeNode constructor.
                    _ => Ok(None),
                }
            }

            // Raw Value::Int in type position — produced when a literal integer appears
            // directly in a TypeNode sequence (e.g., inside TypeNode.Union.types).
            // S-991: literals in type position produce IntLit/StrLit/FloatLit TypeValues.
            Value::Int { n, .. } => Ok(Some(make_typevalue_int_lit(*n))),
            Value::String {
                source, start, end, ..
            } => Ok(Some(make_typevalue_str_lit(&source[*start..*end]))),
            // Raw Value::Float in type position — produced when a float literal appears
            // directly in a TypeNode sequence (e.g., @[or 1.5 2.5] in type-stage evaluation).
            // Parallel to Value::Int handling above.
            Value::Float { n, .. } => Ok(Some(make_typevalue_float_lit(*n))),

            // Constructor dict — a tinct ADT declaration like `Color: [type Red Green Blue]`
            // evaluates to `{ Red: Variant("Color.Red"), Green: Variant("Color.Green"), ... }`.
            // Detect the pattern: all values are Variants sharing the same qualified prefix.
            // If so, return TypeValue.Op(prefix) — the name of the declared type.
            //
            // B-663: when a non-ADT dict is produced by type-stage evaluation for an `@Dict`
            // annotation (e.g., the TypeNode ADT dict when thunks are unsettled, or any other
            // raw Value::Dict appearing in TypeNode position), treat it as the plain Dict type
            // (TypeValue.Repr{repr:"Value::Dict"}) rather than returning Ok(None) which causes
            // downstream callers to interpret the dict's entries as record field types.
            Value::Dict { entries, .. } if !entries.is_empty() => {
                let mut prefix: Option<String> = None;
                let mut all_match = true;
                for (_key, thunk) in entries {
                    match thunk.peek_result() {
                        Some(Ok(val)) => {
                            match val {
                                Value::Variant { ref ctor, .. } => {
                                    let tycon_name =
                                        crate::value::tycon_name_from_ctor(ctor.as_ref())
                                            .to_string();
                                    match &prefix {
                                        None => prefix = Some(tycon_name),
                                        Some(existing)
                                            if existing.as_str() == tycon_name.as_str() => {}
                                        _ => {
                                            all_match = false;
                                            break;
                                        }
                                    }
                                }
                                // Function entries (payload constructors) — still count as the same ADT
                                Value::Function { .. } | Value::Builtin { .. } => {}
                                _ => {
                                    all_match = false;
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                        None => {
                            // Thunk not yet materialized — can't determine
                            all_match = false;
                            break;
                        }
                    }
                }
                if all_match {
                    match prefix {
                        Some(name) => Ok(Some(make_typevalue_op(&name))),
                        None => {
                            // all_match = true but no Variant entries — all entries are
                            // Functions/Builtins (payload constructors). This is an ADT
                            // constructor dict whose tycon name cannot be determined from
                            // settled entries. Treat as the plain Dict type rather than
                            // returning Ok(None) which would fall through to as_type_dispatch
                            // and produce a confusing "unrecognized value" error.
                            Ok(Some(make_typevalue_repr(REPR_DICT)))
                        }
                    }
                } else {
                    // Not an ADT-constructor dict — this is a plain dict value in TypeNode
                    // position (e.g., the Dict from the TypeNode type-stage scope when some
                    // thunks are unsettled). Treat as the plain Dict type.
                    Ok(Some(make_typevalue_repr(REPR_DICT)))
                }
            }

            // Empty Value::Dict in TypeNode position — treat as the plain Dict type.
            // This covers the case where an empty dict is the result of type-stage evaluation
            // for an @Dict annotation.
            Value::Dict { .. } => Ok(Some(make_typevalue_repr(REPR_DICT))),

            // Not a recognizable TypeNode or ADT value.
            _ => Ok(None),
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
/// 1. Looks up [`AS_TYPENODE_PROTOCOL_NAME`] in `state.type_stage_fns` (Function thunks).
/// 2. Materialises the thunk via `state.eval_ctx` to obtain the resolver function value.
/// 3. Invokes the function inline via `evaluate_resolver_by_value` with the TypeNode value as
///    the sole argument. Returns a normalised TypeNode.
/// 4. Converts the normalised TypeNode value to a TypeValue via `typenode_value_to_type`.
///
/// Returns `Ok(None)` if:
/// - `state.type_stage_fns` does not contain `as-typenode` (no type-stage configured).
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
/// `TypeDiagnostic` via `.map_err`.
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
) -> crate::error::EvalResult<Option<TypeValue>> {
    // Step 1: locate the `AS_TYPENODE_PROTOCOL_NAME` resolver in type_stage_fns.
    // This function is a parameterized type constructor that dispatches TypeNode values.
    let resolver_thunk = {
        let scope_non_empty = !state.type_stage_fns.is_empty()
            || state.type_stage_scope.iter().any(|s| !s.is_empty())
            || !state.type_stage_type_vars.is_empty();
        match state.type_stage_fns.get(AS_TYPENODE_PROTOCOL_NAME).cloned() {
            Some(t) => t,
            None => {
                // If the type-stage scope is non-empty but does not define the
                // `as-typenode` protocol function, this is a protocol violation (D-7):
                // any compliant prelude must provide this function.
                if scope_non_empty {
                    let diag_span = rust_span!();
                    state.diagnostics.push(crate::error::TypeDiagnostic::error(
                        "protocol-violation",
                        format!(
                            "type-stage scope does not define the `{}` protocol function (D-7): \
                             any compliant prelude must export this function for TypeNode dispatch",
                            AS_TYPENODE_PROTOCOL_NAME
                        ),
                        diag_span,
                    ));
                }
                return Ok(None);
            }
        }
    };

    // Step 2: materialise the thunk to get the resolver function value.
    let eval_ctx = match state.eval_ctx.clone() {
        Some(ctx) => ctx,
        None => return Ok(None),
    };
    let fn_val = crate::eval::materialize(&resolver_thunk, None, &eval_ctx).await?;

    // Step 3: call the resolver with the TypeNode value as its sole argument.
    // Invoke inline rather than via a helper so the result conversion uses
    // typenode_value_to_type ONLY (no as_type_dispatch fallback), preventing infinite
    // recursion if the resolver itself returns an unrecognised tag.
    let origin_span = rust_span!();
    let arg_thunk = Arc::new(Thunk::value(val.clone(), origin_span.clone()));

    let result_thunk = match fn_val {
        Value::Function {
            ref params,
            ref body,
            ref closure_env,
            ..
        } => {
            if params.len() != 1 {
                return Ok(None); // AS_TYPENODE_PROTOCOL_NAME must be a single-parameter function
            }
            let call_ctx = crate::eval_call::CallContext {
                params,
                body,
                closure_env: Arc::clone(closure_env),
                positional: &[arg_thunk],
                named: None,
                call_span: origin_span.clone(),
                ctx: &eval_ctx,
            };
            crate::eval_call::invoke_function(&call_ctx).await?
        }
        _ => return Ok(None),
    };

    let result_val = crate::eval::materialize(&result_thunk, None, &eval_ctx).await?;

    // Convert using typenode_value_to_type ONLY — no as_type_dispatch fallback here.
    // This prevents infinite recursion: if the resolver itself returns an unrecognised tag,
    // we return Ok(None) rather than attempting further dispatch.
    // Pass empty type_stage_scope: as_type_dispatch is itself the dispatch path for
    // unrecognised TypeConstructor names; recursing into scope lookup here would loop.
    typenode_value_to_type(&result_val, &eval_ctx, &[]).await
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
/// Returns `Err(TypeDiagnostic)` if:
/// - Evaluation of the expression fails (runtime error in type-stage code).
/// - The evaluated value cannot be converted to a Type (unrecognized TypeNode tag).
///
/// Errors propagate to callers. `resolve_property_dict_as_record` falls back to
/// type-stage evaluation when type-dict resolution fails; on eval failure it returns
/// the eval error as the primary diagnostic with the dict-resolution error as a note.
pub(crate) async fn eval_type_stage_expr(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
) -> Result<TypeValue, TypeDiagnostic> {
    let node_span = node.span.clone();

    // Build the EvalContext for evaluating the annotation node.
    // Prefer state.eval_ctx (threaded from the loader pipeline) so type-stage evaluation
    // uses the same capabilities and root scope as the surrounding evaluation. This covers
    // the production path (builtin-typecheck-doc) and the bootstrap path (imports.rs).
    // new_empty(): type-stage evaluation is pure compute — no filesystem I/O occurs.
    let ctx: std::sync::Arc<crate::eval::EvalContext> = if let Some(ref eval_ctx) = state.eval_ctx {
        std::sync::Arc::clone(eval_ctx)
    } else {
        crate::eval::EvalContext::new_empty()
    };

    // No resolution pass for synthetic type-stage nodes; resolution is inline on nodes
    // (written at definition time). Names resolve via the env chain at eval time.
    // All type annotations are inline on AST nodes — no external tables needed.

    // Lower the SurfaceNode to CoreExpr, then wrap as a CoreExpr thunk for lazy evaluation.
    // scope_frames is None here (no loader frames in type-stage context); lower() uses
    // inline resolution coordinates written on nodes at definition time.
    let (lowered, lower_diags) = crate::lower::lower(node, None);
    if let Some(lower_err) = crate::eval_materialize::lower_errors_to_eval_error(lower_diags) {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!("type-stage expression lowering failed: {lower_err}"),
            node_span.clone(),
        ));
    }
    // Use an empty GroupSpine as the root frame. Type-stage annotations resolve names
    // through the EvalContext (ctx) which carries all accumulated entries via its
    // root_group and scope_frames. An explicit GroupSpine is not needed here.
    let root_frame = {
        let group = crate::value::GroupSpine::empty();
        std::sync::Arc::new(crate::value::EvalFrame {
            group,
            closure_env: crate::value::GroupSpine::empty(),
            params: std::sync::Arc::new(vec![]),
        })
    };
    let core_thunk = Arc::new(Thunk::core_expr(
        Arc::new(lowered),
        root_frame,
        Arc::clone(&ctx),
        node_span.clone(),
    ));

    // Materialize — type-stage evaluation is pure compute, no I/O.
    let typenode_val = crate::eval::materialize(&core_thunk, Some(&node_span), &ctx)
        .await
        .map_err(|e| {
            TypeDiagnostic::error(
                "type-error",
                format!("type-stage expression evaluation failed: {e}"),
                node_span.clone(),
            )
        })?;

    // Convert the materialized TypeNode Value to a Type.
    // If the direct structural conversion fails (unrecognised TypeNode tag), try
    // as_type_dispatch to normalise the value through the prelude's `as-typenode`
    // function before giving up.
    // Propagate eval errors from typenode_value_to_type and as_type_dispatch as TypeDiagnostics.
    match typenode_value_to_type(&typenode_val, &ctx, &state.type_stage_scope)
        .await
        .map_err(|e| {
            TypeDiagnostic::error(
                "type-error",
                format!("type-stage field materialization failed: {e}"),
                node_span.clone(),
            )
        })? {
        Some(ty) => return Ok(ty),
        None => {}
    }
    match as_type_dispatch(&typenode_val, state).await.map_err(|e| {
        TypeDiagnostic::error(
            "type-error",
            format!("type-stage field materialization failed: {e}"),
            node_span.clone(),
        )
    })? {
        Some(ty) => return Ok(ty),
        None => {}
    }
    Err(TypeDiagnostic::error(
        "type-error",
        format!("type-stage expression produced an unrecognized value: {typenode_val}"),
        node_span,
    ))
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
) -> Result<Option<TypeValue>, TypeDiagnostic> {
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
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
        ));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(TypeDiagnostic::error(
            "type-error",
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
    // - SurfaceExpression::Dict(entries) — standard syntax: `[$a $b]` or `[$Number]`
    //   (unnamed params) or new syntax: `[x: String  y: Bool]` (named params)
    // - SurfaceExpression::Call { implied: true, func, args } — bare identifiers like `[a b]`
    //   parse as implied calls. Extract func + args as the parameter type expressions.
    let mut params = Vec::new();
    match &second.node.value.expr {
        SurfaceExpression::Dict(param_entries) => {
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
                params.push((param_name, param_ty));
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
            params.push((None, param_ty));
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
                params.push((None, param_ty));
            }
        }
        _ => {
            return Err(TypeDiagnostic::error(
                "type-error",
                "function type parameter list must be a bracket expression",
                second.node.value.span.clone(),
            ))
        }
    }

    Ok(Some(make_typevalue_fn(params, ret)))
}
