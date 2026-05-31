//! Type annotation resolution and type expression parsing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::types::{Constraint, InferState, Kind, Row, Type, TypeAlias, TypeEnv, TypeError};

pub(crate) fn expand_type_alias(
    inner: &Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
) -> Result<Type, TypeError> {
    // Use a fresh per-alias mapping so annotation names within one type alias expression
    // (e.g., `a` in `[Fn@a [a]]`) consistently map to the same fresh TypeVar.
    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
    // The `let _ = resolve_type_expr(...)` discards the resolved type intentionally — the call
    // is for validation side-effects (error propagation) only. Standalone type alias expressions
    // have no runtime type; returning Any is correct. The actual type alias definition is
    // registered in the TypeEnv during dict inference (see infer_dict Pass 2).
    let _ = resolve_type_expr(inner, env, state, &mut Some(&mut alias_ann_map), &mut None)?;
    Ok(Type::Unknown)
}

pub(crate) fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Arc<SurfaceNode>,
    resolved_type: &RefCell<Option<Type>>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Create per-annotation-scope mappings for type and row variables.
    // Named row variables (e.g., ...r) in TypeAssert annotations are tracked correctly
    // instead of creating fresh anonymous row vars.
    let mut ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
    let mut ann_mapping_opt = ann_mapping.as_mut();
    let mut row_ann_mapping_opt: Option<&mut HashMap<String, String>> = None;

    let expected = resolve_annotation(
        &annotation.node,
        env,
        annotation.span.clone(),
        state,
        &mut ann_mapping_opt,
        &mut row_ann_mapping_opt,
    )
    .map_err(|e| vec![e])?;

    // resolved_type will be stored after substitution application below (write-once invariant).

    // Use checking mode for TypeAssert inner expression (doc/06 §Bidirectional Typing).
    let check_result = check_surface_expr(inner, &expected, env, state, type_map);

    // If checking fails, propagate errors (TypeAssert failures are hard type errors).
    if let Err(type_errors) = check_result {
        let has_default = annotation.node.get_property("default").is_some();
        if !has_default {
            return Err(type_errors);
        }
    }

    // Validate the default value type — hard error if the default cannot satisfy the asserted type.
    if let Some(default_node) = annotation.node.get_property("default") {
        match infer_surface_expr(default_node, env, state, type_map) {
            Ok(default_ty) => {
                // Apply state.subst to both types before comparison — access-chain constraints
                // may have bound TypeVars in state.subst (e.g., $data.name generates row-variable
                // bindings). Without substitution, the comparison uses stale TypeVars.
                // Guard: skip allocation when subst is empty (common case for concrete programs).
                let (default_ty, expected_resolved) = if state.subst.is_empty() {
                    (default_ty, expected.clone())
                } else {
                    (state.subst.apply(&default_ty), state.subst.apply(&expected))
                };
                let passes = Type::is_subtype(&default_ty, &expected_resolved)
                    || ((contains_unknown_or_top(&default_ty)
                        || contains_unknown_or_top(&expected_resolved))
                        && Type::is_consistent(&default_ty, &expected_resolved));
                if !passes {
                    return Err(vec![TypeError::new(
                        format!(
                            "default value type mismatch: default has type {default_ty}, \
                             but assertion expects {expected_resolved}"
                        ),
                        default_node.span.clone(),
                    )]);
                }
            }
            Err(errs) => {
                // Propagate inference errors from the default expression
                return Err(errs);
            }
        }
    }

    // Validate repr: storage hint if present.
    // Valid values: "u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64".
    // Must be consistent with the declared type (must be numeric).
    if let Some(repr_node) = annotation.node.get_property("repr") {
        if let SurfaceExpression::Str(ref repr_val) = repr_node.expr {
            const VALID_REPRS: &[&str] = &["u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64"];
            if !VALID_REPRS.contains(&repr_val.as_str()) {
                return Err(vec![TypeError::new(
                    format!(
                        "invalid repr: \"{repr_val}\" — must be one of: {}",
                        VALID_REPRS.join(", ")
                    ),
                    repr_node.span.clone(),
                )]);
            }
            // Check consistency: repr requires a numeric type (Int or Number)
            let is_numeric = matches!(&expected, Type::Int | Type::Number | Type::Float);
            if !is_numeric {
                return Err(vec![TypeError::new(
                    format!(
                        "repr: \"{repr_val}\" requires a numeric type, but annotation declares {}",
                        expected
                    ),
                    repr_node.span.clone(),
                )]);
            }
        }
    }

    // Apply substitution before returning to ensure bound type variables are resolved.
    // The expected type may contain TypeVars that were bound during checking mode or
    // access-chain inference (e.g., check_dot_access binds row variables).
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let expected = if state.subst.is_empty() {
        expected
    } else {
        state.subst.apply(&expected)
    };

    // Store the substitution-applied type in the AST node for runtime validation (elaboration).
    // INVARIANT: resolved_type is write-once (parser initializes to None, typecheck sets it once).
    // The type is stored AFTER substitution to ensure the runtime sees fully-resolved types.
    //
    // GUARD: parameterized types like `Handle[Readable]` can trigger a second write when the
    // type checker encounters the same TypeAssert node via multiple paths (e.g., dict letrec
    // re-inference). If the second write is consistent (same type), skip it silently — this is
    // an idempotent double-elaboration, not a true invariant violation. If the types differ,
    // that IS a real bug and we return an internal error.
    let prev = resolved_type.replace(Some(expected.clone()));
    if let Some(prev_type) = prev {
        // Type does not implement PartialEq — use debug representations for consistency check.
        let prev_repr = format!("{prev_type:?}");
        let new_repr = format!("{expected:?}");
        if prev_repr != new_repr {
            return Err(vec![TypeError::new(
                format!(
                    "internal error: resolved_type written twice with inconsistent types — \
                     elaboration invariant violated (previous: {prev_repr}, new: {new_repr}). \
                     This indicates a bug in the type checker."
                ),
                annotation.span.clone(),
            )]);
        }
        // Consistent double-write: previous value matches. Idempotent — return Ok.
    }

    Ok(expected)
}

/// Resolve an annotated type expression `[@Name $annotation]`.
///
/// If `name == "Fn"`, interprets `$annotation` as a function type specification:
/// - `[@Fn@RetType [Param1 Param2 ...]]` → function type with params and return type
/// - `[@Fn@RetType]` (no param list) → zero-parameter function returning RetType
///
/// If `name == "Seq"`, interprets `$annotation` as the element type:
/// - `Seq@ElemType` (bare Annotated form) → `Type::Seq(ElemType)`
/// - `[@Seq expr]` (TypeAssert) → checks `expr` against `Type::Seq(Any)` (element type is Any; `@ElemType` suffix is a parse error in TypeAssert position)
///
/// Otherwise, resolves `$annotation` as a regular type annotation.
pub(crate) fn resolve_annotated(
    name: &str,
    annotation: &Spanned<Annotation>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if name == "Fn" {
        resolve_fn_type(
            &annotation.node,
            env,
            annotation.span.clone(),
            state,
            ann_mapping,
            row_ann_mapping,
        )
    } else if name == "Seq" {
        let elem = resolve_annotation(
            &annotation.node,
            env,
            span.clone(),
            state,
            ann_mapping,
            row_ann_mapping,
        )?;
        let ty = Type::Seq(Box::new(elem));
        crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
        Ok(ty)
    } else if name == "Handle" {
        // Handle@SomeCapType — subscript form for capability row in parameter annotations.
        //
        // The inner annotation is the capability row argument. Examples:
        //   h@Handle@DirCap         → Handle(DirCap)
        //   h@Handle@NetCap         → Handle(NetCap)
        //   h@Handle@[Readable]     → Handle(Record { readable: {} })
        //   h@Handle@Unknown        → Handle(Unknown)  (gradual handle)
        //
        // Resolve the inner annotation as a type and wrap in Handle.
        let cap_type = resolve_annotation(
            &annotation.node,
            env,
            span,
            state,
            ann_mapping,
            row_ann_mapping,
        )?;
        Ok(Type::Handle(Box::new(cap_type)))
    } else {
        resolve_annotation(
            &annotation.node,
            env,
            span,
            state,
            ann_mapping,
            row_ann_mapping,
        )
    }
}

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
pub(crate) fn resolve_fn_metadata(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<(Type, Option<String>), TypeError> {
    // Valid hardcoded class names (pre-HKT)
    const VALID_CLASSES: &[&str] = &[
        "Equatable",
        "Comparable",
        "Numeric",
        "Showable",
        "Mappable",
        "Appendable",
        "HasField",
    ];

    let mut return_type: Option<Type> = None;
    let mut doc_string: Option<String> = None;

    // Step 0: Process bind: entries (must come first so TypeVars exist for return:/constraint:/kinds:)
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
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
                                    return Err(TypeError::new(
                                        "bind: list must contain only positional entries (bare names)",
                                        bind_entry.span.clone(),
                                    ));
                                }
                                match &bind_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Check lowercase convention for TypeVar names
                                        if !name.starts_with(|c: char| c.is_lowercase()) {
                                            return Err(TypeError::new(
                                                format!(
                                                    "bind: TypeVar name '{}' must start with lowercase letter",
                                                    name
                                                ),
                                                bind_entry.node.value.span.clone(),
                                            ));
                                        }
                                        // Create fresh TypeVar and register in ann_mapping
                                        let fresh = format!("_t{}", state.name_counter);
                                        state.name_counter = state.name_counter.saturating_add(1);
                                        state.levels.insert(fresh.clone(), state.level);
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), name.clone());
                                        if let Some(ref mut mapping) = ann_mapping {
                                            mapping.insert(name.clone(), fresh);
                                        } else {
                                            return Err(TypeError::new(
                                                "bind: requires an annotation mapping context",
                                                span,
                                            ));
                                        }
                                    }
                                    _ => {
                                        return Err(TypeError::new(
                                            "bind: entries must be bare names (TypeVar names)",
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
                                return Err(TypeError::new(
                                    "bind: list must contain only bare names, not named arguments",
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
                                            return Err(TypeError::new(
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
                                            _ => return Err(TypeError::new(
                                                "bind: entries must be bare names (TypeVar names)",
                                                arg.span.clone(),
                                            )),
                                        }
                                    }
                                    v
                                };
                            for (name, name_span) in all_names {
                                if !name.starts_with(|c: char| c.is_lowercase()) {
                                    return Err(TypeError::new(
                                        format!(
                                            "bind: TypeVar name '{}' must start with lowercase letter",
                                            name
                                        ),
                                        name_span,
                                    ));
                                }
                                let fresh = format!("_t{}", state.name_counter);
                                state.name_counter = state.name_counter.saturating_add(1);
                                state.levels.insert(fresh.clone(), state.level);
                                // Register source name for better T013 diagnostics
                                state
                                    .type_var_source_names
                                    .insert(fresh.clone(), name.to_string());
                                if let Some(ref mut mapping) = ann_mapping {
                                    mapping.insert(name.to_string(), fresh);
                                } else {
                                    return Err(TypeError::new(
                                        "bind: requires an annotation mapping context",
                                        span,
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(TypeError::new(
                                "bind: value must be a list [a b c]",
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
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if key_name == "kinds" {
                    // kinds: [f: Operator key: Label] — dict mapping TypeVar names to kinds
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict(kinds_entries) => {
                            for kind_entry in kinds_entries {
                                let typevar_name =
                                    match &kind_entry.node.key {
                                        Some(k) => match &k.expr {
                                            SurfaceExpression::Str(s) => s.clone(),
                                            _ => return Err(TypeError::new(
                                                "kinds: keys must be bare words (TypeVar names)",
                                                kind_entry.span.clone(),
                                            )),
                                        },
                                        None => {
                                            return Err(TypeError::new(
                                                "kinds: entries must be keyed [name: kind]",
                                                kind_entry.span.clone(),
                                            ))
                                        }
                                    };

                                // Validate that this name was declared in bind:
                                let type_var = if let Some(ref mapping) = ann_mapping {
                                    match mapping.get(&typevar_name) {
                                        Some(var) => var.clone(),
                                        None => {
                                            return Err(TypeError::new(
                                                format!(
                                                    "kinds: TypeVar '{}' not found in bind: list",
                                                    typevar_name
                                                ),
                                                kind_entry.span.clone(),
                                            ))
                                        }
                                    }
                                } else {
                                    return Err(TypeError::new(
                                        "kinds: requires an annotation mapping context",
                                        span,
                                    ));
                                };

                                // Parse the kind name
                                match &kind_entry.node.value.expr {
                                    SurfaceExpression::VarRef {
                                        name: kind_name, ..
                                    } => {
                                        let kind = match kind_name.as_str() {
                                            "Operator" => Kind::Operator,
                                            "Label" => Kind::Label,
                                            _ => {
                                                return Err(TypeError::new(
                                                    format!(
                                                    "unknown kind '{}' (valid: Operator, Label)",
                                                    kind_name
                                                ),
                                                    kind_entry.node.value.span.clone(),
                                                ))
                                            }
                                        };
                                        state.kind_env.insert(type_var, kind);
                                    }
                                    _ => {
                                        return Err(TypeError::new(
                                            "kinds: value must be a kind name (Operator or Label)",
                                            kind_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeError::new(
                                "kinds: value must be a dict [name: kind ...]",
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
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if key_name == "constraint" {
                    // constraint: [a: Comparable] or [a: [each Comparable Showable]]
                    match &entry.node.value.expr {
                        SurfaceExpression::Dict(constraint_entries) => {
                            for c_entry in constraint_entries {
                                // Skip positional entries (MPTC) — handled in Step 1b
                                if c_entry.node.key.is_none() {
                                    continue;
                                }

                                let typevar_name = match &c_entry.node.key {
                                    Some(k) => match &k.expr {
                                        SurfaceExpression::Str(s) => s.clone(),
                                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                                        _ => {
                                            return Err(TypeError::new(
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
                                        let fresh = format!("_t{}", state.name_counter);
                                        state.name_counter = state.name_counter.saturating_add(1);
                                        state.levels.insert(fresh.clone(), state.level);
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), typevar_name.clone());
                                        mapping.insert(typevar_name.clone(), fresh.clone());
                                        fresh
                                    }
                                } else {
                                    return Err(TypeError::new(
                                        "constraint annotations require an annotation mapping context",
                                        span,
                                    ));
                                };

                                // Parse the class name(s) — can be a single name, [each ...], or [...]
                                // The parser represents `[each Comparable Showable]` as
                                // SurfaceExpression::Call { func: VarRef("each"),
                                // args: [VarRef("Comparable"), VarRef("Showable")] }.
                                // We accept both Dict form (legacy) and Call form (natural parse).
                                match &c_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Single class: [a: Comparable]
                                        if !VALID_CLASSES.contains(&name.as_str())
                                            && state.class_env.get(name).is_none()
                                        {
                                            return Err(TypeError::new(
                                                format!("unknown constraint class '{}'", name),
                                                c_entry.node.value.span.clone(),
                                            ));
                                        }
                                        state.add_constraint(name.clone(), type_var.clone());
                                    }
                                    SurfaceExpression::Dict(class_list) => {
                                        // Require [each ...] keyword form
                                        let class_entries = if !class_list.is_empty()
                                            && class_list[0].node.key.is_none()
                                        {
                                            if let SurfaceExpression::VarRef { name, .. } =
                                                &class_list[0].node.value.expr
                                            {
                                                if name == "each" {
                                                    // [a: [each Comparable Showable]] — skip 'each'
                                                    &class_list[1..]
                                                } else {
                                                    // [a: [Comparable Showable]] — no 'each', error
                                                    return Err(TypeError::new(
                                                        "constraint class list must start with 'each' keyword: use [each ClassName ...]",
                                                        class_list[0].span.clone(),
                                                    ));
                                                }
                                            } else {
                                                return Err(TypeError::new(
                                                    "constraint class list must start with 'each' keyword: use [each ClassName ...]",
                                                    class_list[0].span.clone(),
                                                ));
                                            }
                                        } else {
                                            return Err(TypeError::new(
                                                "constraint class list cannot be empty",
                                                c_entry.node.value.span.clone(),
                                            ));
                                        };

                                        // Multiple classes: iterate and add each
                                        for class_entry in class_entries {
                                            if class_entry.node.key.is_some() {
                                                return Err(TypeError::new(
                                                    "constraint class list must contain only positional entries",
                                                    class_entry.span.clone(),
                                                ));
                                            }
                                            match &class_entry.node.value.expr {
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    if !VALID_CLASSES.contains(&name.as_str())
                                                        && state.class_env.get(name).is_none()
                                                    {
                                                        return Err(TypeError::new(
                                                            format!(
                                                                "unknown constraint class '{}'",
                                                                name
                                                            ),
                                                            class_entry.node.value.span.clone(),
                                                        ));
                                                    }
                                                    state.add_constraint(
                                                        name.clone(),
                                                        type_var.clone(),
                                                    );
                                                }
                                                _ => {
                                                    return Err(TypeError::new(
                                                        "constraint class must be a class name (e.g., Comparable)",
                                                        class_entry.node.value.span.clone(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    // Call form: `[each Comparable Showable]` →
                                    // Call(VarRef("each"), [VarRef("Comparable"), VarRef("Showable")]).
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
                                            return Err(TypeError::new(
                                                "constraint class list must not contain named arguments",
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
                                                                return Err(TypeError::new(
                                                                    "constraint class must be a class name (e.g., Comparable)",
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
                                                    return Err(TypeError::new(
                                                        "constraint value must be a class name or [each Class1 Class2 ...]",
                                                        c_entry.node.value.span.clone(),
                                                    ))
                                                }
                                            };
                                        for (name, name_span) in class_names {
                                            if !VALID_CLASSES.contains(&name)
                                                && state.class_env.get(name).is_none()
                                            {
                                                return Err(TypeError::new(
                                                    format!("unknown constraint class '{}'", name),
                                                    name_span,
                                                ));
                                            }
                                            state
                                                .add_constraint(name.to_string(), type_var.clone());
                                        }
                                    }
                                    _ => {
                                        return Err(TypeError::new(
                                            "constraint value must be a class name or list of class names",
                                            c_entry.node.value.span.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeError::new(
                                "constraint: value must be a dict [a: Comparable]",
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
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
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

                                        // Validate the class exists in ClassEnv and get the ClassDecl
                                        let class_decl =
                                            state.class_env.get(class_name).ok_or_else(|| {
                                                TypeError::new(
                                                    format!(
                                                        "unknown class '{}' in MPTC constraint",
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
                                                            return Err(TypeError::new(
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
                                                        return Err(TypeError::new(
                                                            "constraint annotations require an annotation mapping context",
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
                                                    return Err(TypeError::new(
                                                        "MPTC constraint entries after class name must be TypeVar names",
                                                        subsequent.node.value.span.clone(),
                                                    ));
                                                }
                                            }
                                            j += 1;
                                        }

                                        // Create the MPTC constraint using Arc<ClassDecl>
                                        state.constraints.push(Constraint::Class {
                                            class: Arc::new(class_decl.clone()),
                                            vars: typevar_names,
                                            origin_name: None,
                                            origin_span: None,
                                        });

                                        // Skip the entries we just processed (the class name + TypeVars)
                                        i = j;
                                    }
                                    _ => {
                                        // Non-escaped positional entry that's not part of an MPTC
                                        // This is probably an error — bare positional TypeVar names
                                        // without a class
                                        return Err(TypeError::new(
                                            "positional constraint entries must start with escaped class name (e.g., $Add)",
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
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if key_name == "return" {
                    let ret = resolve_type_expr(
                        &entry.node.value,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
                    return_type = Some(ret);
                }
            }
        }
    }

    // Step 3: Process doc: entry
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if key_name == "doc" {
                    // Accept both plain strings and unindent(...) calls (from triple-quoted strings).
                    // Triple-quoted strings `"""..."""` are desugared by the parser to
                    // `Call { func: VarRef("unindent"), args: [Str(s)] }`.
                    let extracted = match &entry.node.value.expr {
                        SurfaceExpression::Str(s) => Some(s.clone()),
                        SurfaceExpression::Call { func, args, .. } => {
                            if matches!(&func.expr,
                                SurfaceExpression::VarRef { name, .. } if name == "unindent")
                            {
                                args.iter().find_map(|arg| {
                                    if let SurfaceExpression::Str(s) = &arg.expr {
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
                            return Err(TypeError::new(
                                "doc: value must be a string literal",
                                entry.node.value.span.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Step 4: Check for unknown keys
    const VALID_KEYS: &[&str] = &["return", "constraint", "doc", "bind", "kinds"];
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if !VALID_KEYS.contains(&key_name.as_str()) {
                    return Err(TypeError::new(
                        format!(
                            "unknown function annotation key '{}' (valid keys: return, constraint, doc, bind, kinds)",
                            key_name
                        ),
                        key_expr.span.clone(),
                    ));
                }
            }
        }
    }

    // If no return: key, default to Unknown (infer from body)
    let ret = return_type.unwrap_or(Type::Unknown);

    Ok((ret, doc_string))
}

/// Resolve a bare `Fn@ReturnType` annotation (without parameter list) into a function type.
/// `Fn@T` bare = zero-param function returning T; full function type with params uses `try_resolve_fn_type_expr`.
///
/// For `fn@[...]` PropertyDict annotations, dispatches to:
/// - `resolve_fn_metadata()` if ANY entry has a named key matching `return:`, `constraint:`, or `doc:`
/// - existing union return type path if ALL entries are positional
/// - error if mixed named + positional
fn resolve_fn_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::PropertyDict(surface_entries) => {
            // Dispatch: if any entry has a named key matching function metadata keys
            // (return:, constraint:, doc:, bind:, kinds:), treat as fn metadata dict.
            // If all entries are positional, delegate to resolve_type_dict (handles
            // [Fn@Return [Params]] and union-style type expressions).
            let has_fn_key = surface_entries.iter().any(|e| {
                if let Some(ref key) = e.node.key {
                    matches!(&key.expr,
                        SurfaceExpression::Str(s)
                            if matches!(s.as_str(),
                                "return" | "constraint" | "doc" | "bind" | "kinds"))
                } else {
                    false
                }
            });
            // Check if all entries are keyed (no positional entries)
            let all_keyed = surface_entries.iter().all(|e| e.node.key.is_some());

            if has_fn_key {
                // Mixed keys validation: if we have fn annotation keys, all entries must be keyed
                if !all_keyed {
                    return Err(TypeError::new(
                        "fn annotation must use either named keys (return:, constraint:, doc:, bind:, kinds:) or positional entries (union return type), not both",
                        span,
                    ));
                }
                let (ret, _doc) = resolve_fn_metadata(
                    surface_entries,
                    env,
                    span.clone(),
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )?;
                let ty = Type::Function {
                    params: vec![],
                    ret: Box::new(ret),
                    variadic: false,
                };
                crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
                Ok(ty)
            } else {
                // All-positional or record-field style — delegate to resolve_type_dict.
                // Handles [Fn@Return [Params]] (detected by try_resolve_fn_type_expr),
                // record types, and type constructors.
                resolve_type_dict(
                    surface_entries,
                    env,
                    span,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            }
        }
        _ => {
            // Simple(name) path: fn@Int, fn@a, etc.
            let ret = resolve_annotation_as_type(
                ann,
                env,
                span.clone(),
                state,
                ann_mapping,
                row_ann_mapping,
            )?;
            let ty = Type::Function {
                params: vec![],
                ret: Box::new(ret),
                variadic: false,
            };
            crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
            Ok(ty)
        }
    }
}

/// Resolve an annotation in a context where a type expression is expected.
/// Unlike `resolve_annotation`, a PropertyDict is interpreted as a type expression
/// (record type or function type) rather than a property bag.
fn resolve_annotation_as_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(name, env, span, state, ann_mapping, &row_ref)
        }
        Annotation::PropertyDict(surface_entries) => {
            // In a type-expression context, a PropertyDict is always a structural type:
            // a record type [field: Type ...], a function type [Fn@Return [Params]],
            // or a type constructor application [Seq Int], [or A B], etc.
            // Delegate to resolve_type_dict which handles all these forms.
            resolve_type_dict(
                surface_entries,
                env,
                span,
                state,
                ann_mapping,
                row_ann_mapping,
            )
        }
        Annotation::Annotated(name, inner) => {
            // For fn annotations, forward to full resolver
            // (e.g., fn@Seq@Int should resolve the Annotated properly)
            resolve_annotation(
                &Annotation::Annotated(name.clone(), inner.clone()),
                env,
                span,
                state,
                ann_mapping,
                row_ann_mapping,
            )
        }
    }
}

pub(crate) fn resolve_annotation(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(name, env, span, state, ann_mapping, &row_ref)
        }
        Annotation::Annotated(name, inner) => {
            // Parameterized type annotations: Seq@Int, Map@[String: Int], Record@[field: Type]
            match name.as_str() {
                "Seq" => {
                    // Resolve the inner type
                    let elem_type =
                        resolve_annotation(inner, env, span, state, ann_mapping, row_ann_mapping)?;
                    Ok(Type::Seq(Box::new(elem_type)))
                }
                "Map" => {
                    // Resolve the inner annotation for key and value types
                    match inner.as_ref() {
                        Annotation::Simple(_) => {
                            // @Map@T (single type) → Map[fresh_key: T]
                            // Use a fresh TypeVar for the key so callers can unify against
                            // concrete key types instead of being stuck with Unknown.
                            let value_type = resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            Ok(Type::Map(
                                Box::new(state.fresh_type_var()),
                                Box::new(value_type),
                            ))
                        }
                        Annotation::PropertyDict(surface_entries) => {
                            // @Map@[key: K value: V] → Map(K, V)
                            // Look for keyed entries named "key" and "value". If both are
                            // present, resolve them and build Map(K, V). If only "value" (or
                            // any single positional entry), treat as Map(Unknown, V).
                            // Fall back to resolving as a positional type list for other forms.
                            let key_entry = surface_entries.iter().find(|e| {
                                e.node.key.as_ref().is_some_and(
                                    |k| matches!(&k.expr, SurfaceExpression::Str(s) if s == "key"),
                                )
                            });
                            let value_entry = surface_entries.iter().find(|e| {
                                e.node.key.as_ref().is_some_and(
                                    |k| matches!(&k.expr, SurfaceExpression::Str(s) if s == "value"),
                                )
                            });
                            if let Some(v_entry) = value_entry {
                                let key_ty = if let Some(k_entry) = key_entry {
                                    resolve_type_expr(
                                        &k_entry.node.value,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )?
                                } else {
                                    Type::Unknown
                                };
                                let value_ty = resolve_type_expr(
                                    &v_entry.node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)))
                            } else {
                                // No "value:" key — delegate to resolve_type_dict which handles
                                // positional forms like [Map K V] (though nested inside @Map@).
                                resolve_type_dict(
                                    surface_entries,
                                    env,
                                    span,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )
                            }
                        }
                        _ => {
                            // Other forms like @Map@Annotated — treat as single value type.
                            // Use a fresh TypeVar for the key so callers can unify against
                            // concrete key types instead of being stuck with Unknown.
                            let value_type = resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            Ok(Type::Map(
                                Box::new(state.fresh_type_var()),
                                Box::new(value_type),
                            ))
                        }
                    }
                }
                "Record" => {
                    // @Record@[field: Type ...]
                    match inner.as_ref() {
                        Annotation::PropertyDict(surface_entries) => {
                            // @Record@[field: Type ...] → structural record type.
                            // Delegate to resolve_type_dict which handles record fields,
                            // row variables (...r), and type constructor applications.
                            resolve_type_dict(
                                surface_entries,
                                env,
                                span,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )
                        }
                        _ => Err(TypeError::new(
                            "Record parameterization requires a dict: @Record@[field: Type ...]",
                            span,
                        )),
                    }
                }
                "Tuple" => {
                    // @Tuple@[T0 T1 T2] → closed record {0: T0, 1: T1, 2: T2}
                    // Parameterized form: takes positional entries as element types.
                    match inner.as_ref() {
                        Annotation::PropertyDict(surface_entries) => {
                            // @Tuple@[T0 T1 T2] → closed record {0: T0, 1: T1, 2: T2}.
                            // All entries must be positional (auto-indexed); each resolves
                            // as an element type in declaration order.
                            let mut fields = HashMap::new();
                            for (idx, entry) in surface_entries.iter().enumerate() {
                                let elem_ty = resolve_type_expr(
                                    &entry.node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                fields.insert(idx.to_string(), elem_ty);
                            }
                            let ty = Type::Record(Row { fields });
                            crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
                            Ok(ty)
                        }
                        _ => {
                            // Single-type form: @Tuple@Int → {0: Int}
                            let elem_ty = resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            let mut fields = HashMap::new();
                            fields.insert("0".to_string(), elem_ty);
                            Ok(Type::Record(Row { fields }))
                        }
                    }
                }
                "Handle" => {
                    // @Handle@CapType — parameterized handle type in TypeAssert/annotation context.
                    //
                    // The inner annotation is the capability row argument. Examples:
                    //   @Handle@DirCap            → Handle(DirCap)
                    //   @Handle@NetCap            → Handle(NetCap)
                    //   @Handle@[Readable]        → Handle(Record { readable: {} })
                    //   @Handle@Unknown           → Handle(Unknown)  (gradual handle)
                    //
                    // Resolve the inner annotation as a capability type and wrap in Handle.
                    let cap_type =
                        resolve_annotation(inner, env, span, state, ann_mapping, row_ann_mapping)?;
                    Ok(Type::Handle(Box::new(cap_type)))
                }
                _ => {
                    // Unknown parameterized type — could be a type alias or error
                    Err(TypeError::new(
                        format!("unknown parameterized type: {}", name),
                        span,
                    ))
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
            // (type:, default:, repr:, doc:). If there are non-metadata keys like "id:", "name:",
            // etc., the annotation must be a structural record type — "type:" is a field name.
            // E.g.: @[type: String] → Type::Str (shorthand)
            //       @[type: String id: Int] → Record{type: Str, id: Int} (structural)
            let metadata_keys = ["type", "default", "repr", "doc"];
            let has_non_metadata_key = surface_entries.iter().any(|se| {
                if let Some(ref k) = se.node.key {
                    match &k.expr {
                        SurfaceExpression::Str(s) => !metadata_keys.contains(&s.as_str()),
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
                        SurfaceExpression::Str(s) if s == "type" => Some(&se.node.value),
                        _ => None,
                    }
                })
            } else {
                None
            };

            if let Some(type_node) = type_value_node {
                // @[type: T ...] shorthand — resolve the type: value as a type expression.
                resolve_type_expr(type_node, env, state, ann_mapping, row_ann_mapping)
            } else if let Some(label_value_node) = surface_entries.iter().find_map(|se| {
                // @[label: name] — named Label-kinded TypeVar annotation.
                // Only fires when there is exactly one entry and its key is "label".
                if surface_entries.len() == 1 {
                    let key_node = se.node.key.as_ref()?;
                    match &key_node.expr {
                        SurfaceExpression::Str(s) if s == "label" => Some(&se.node.value),
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
                    SurfaceExpression::Str(_) => Err(TypeError::new(
                        "label: value must be a bare name (e.g. `label: l`), not a string literal",
                        span,
                    )),
                    SurfaceExpression::VarRef { name, .. } => {
                        if name.starts_with(|c: char| c.is_uppercase()) {
                            Err(TypeError::new(
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
                                    let v = format!("_label_{}", state.name_counter);
                                    state.name_counter = state.name_counter.saturating_add(1);
                                    state.levels.insert(v.clone(), state.level);
                                    state.kind_env.insert(v.clone(), Kind::Label);
                                    state.type_var_source_names.insert(v.clone(), name.clone());
                                    mapping.insert(name.clone(), v.clone());
                                    v
                                }
                            } else {
                                let v = format!("_label_{}", state.name_counter);
                                state.name_counter = state.name_counter.saturating_add(1);
                                state.levels.insert(v.clone(), state.level);
                                state.kind_env.insert(v.clone(), Kind::Label);
                                v
                            };
                            let current_level = *state
                                .levels
                                .get(&fresh)
                                .expect("invariant: label var just inserted into levels");
                            Ok(Type::TypeVar(fresh, current_level))
                        }
                    }
                    _ => Err(TypeError::new(
                        "label: value must be a bare name (e.g. `label: l`)",
                        span,
                    )),
                }
            } else {
                // No "type:" key (or has non-metadata keys) — treat as structural type or metadata.
                resolve_property_dict_as_record(
                    surface_entries,
                    env,
                    span,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            }
        }
    }
}

fn resolve_property_dict_as_record(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    resolve_type_dict(entries, env, span, state, ann_mapping, row_ann_mapping).or_else(|e| {
        if entries_look_like_type_dict(entries) {
            Err(e)
        } else {
            // For PropertyDict entries that don't look like a type dict and aren't recognized
            // type constructors (or/all/without/Seq/Map already handled before this path),
            // fall through with Unknown. This covers genuine metadata-style annotations
            // (e.g., @[default: 42]) that reach this path via resolve_annotation's else branch.
            let _ = e; // suppress the error — annotation is non-structural
            Ok(Type::Unknown)
        }
    })
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
            if let SurfaceExpression::Annotated { name, .. } = &first.node.value.expr {
                if name == "Fn" {
                    return true;
                }
            }
        }
    }

    // Record type pattern: every entry has a string key and a type-shaped value.
    entries.iter().all(|entry| {
        // Rest entries (`...` / `...name`) are valid in type dicts
        if matches!(&entry.node.value.expr, SurfaceExpression::Rest(_)) {
            return true;
        }
        // Every entry must have a string key
        let has_str_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| matches!(&k.expr, SurfaceExpression::Str(_)));
        // Value must be a form that could be a type expression
        let value_is_type_shaped = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Str(_)
                | SurfaceExpression::VarRef { .. }
                | SurfaceExpression::Dict(_)
                | SurfaceExpression::Annotated { .. }
        );
        has_str_key && value_is_type_shaped
    })
}

/// Instantiate a parameterized type alias by substituting type arguments for parameters.
///
/// Given `Pair: [type [a] [first: a second: a]]` and args `[Int]`,
/// builds substitution `{a -> Int}` and applies to body to get `[first: Int second: Int]`.
fn instantiate_type_alias(
    alias: &TypeAlias,
    type_args: &[Type],
    state: &mut InferState,
) -> Result<Type, TypeError> {
    // Build substitution from parameter names to provided types
    let mut type_subst: HashMap<String, Type> = HashMap::new();
    for (param, arg) in alias.params.iter().zip(type_args.iter()) {
        type_subst.insert(param.clone(), arg.clone());
    }

    // Apply substitution to the alias body
    Ok(apply_type_alias_substitution(
        &alias.body,
        &type_subst,
        state,
    ))
}

/// Apply a type-level substitution to a type expression.
///
/// This is distinct from `Substitution::apply` which operates on unification variables.
/// Type alias substitution replaces parameter names with concrete types.
fn apply_type_alias_substitution(
    ty: &Type,
    subst: &HashMap<String, Type>,
    state: &mut InferState,
) -> Type {
    match ty {
        Type::TypeVar(name, _) => {
            // Check if this is a type alias parameter
            if let Some(replacement) = subst.get(name) {
                replacement.clone()
            } else {
                // Not a parameter — keep as-is but refresh the level from state
                let level = *state.levels.get(name).unwrap_or(&state.level);
                Type::TypeVar(name.clone(), level)
            }
        }
        Type::Record(row) => {
            let new_fields: HashMap<String, Type> = row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), apply_type_alias_substitution(v, subst, state)))
                .collect();
            Type::Record(Row { fields: new_fields })
        }
        Type::Function {
            params,
            ret,
            variadic,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, p_ty)| {
                    (
                        name.clone(),
                        apply_type_alias_substitution(p_ty, subst, state),
                    )
                })
                .collect(),
            ret: Box::new(apply_type_alias_substitution(ret, subst, state)),
            variadic: *variadic,
        },
        Type::Seq(elem) => Type::Seq(Box::new(apply_type_alias_substitution(elem, subst, state))),
        Type::Union(members) => Type::Union(
            members
                .iter()
                .map(|m| apply_type_alias_substitution(m, subst, state))
                .collect(),
        ),
        Type::Intersection(members) => Type::normalize_intersection(
            members
                .iter()
                .map(|m| apply_type_alias_substitution(m, subst, state))
                .collect(),
        ),
        Type::Negation(inner) => {
            Type::Negation(Box::new(apply_type_alias_substitution(inner, subst, state)))
        }
        Type::Map(key, value) => Type::Map(
            Box::new(apply_type_alias_substitution(key, subst, state)),
            Box::new(apply_type_alias_substitution(value, subst, state)),
        ),
        // All other types are atomic and don't contain substitutable parameters
        _ => ty.clone(),
    }
}

/// Resolve a type name with recursion guard (used during alias registration).
#[allow(clippy::too_many_arguments)] // Internal helper for type alias expansion
pub(crate) fn resolve_type_name_with_guard(
    name: &str,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    _current_alias: &str,
    depth: usize,
) -> Result<Type, TypeError> {
    // Handle builtin types and lowercase type variables using normal resolution
    if !name.starts_with(|c: char| c.is_uppercase())
        || matches!(
            name,
            "Int"
                | "Float"
                | "String"
                | "Bool"
                | "Number"
                | "Any"
                | "Seq"
                | "Handle"
                | "Null"
                | "Dict"
                | "Map"
                | "Record"
                | "Tuple"
                | "Fn"
                | "Never"
                | "Top"
                | "Unknown"
        )
    {
        let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
        return resolve_type_name(name, env, span, state, ann_mapping, &row_ref);
    }

    // Uppercase type name — check for type alias
    if let Some(alias) = env.get_type_alias(name) {
        // Check if we're in a recursive expansion
        if recursion_guard.contains(name) {
            // Recursive reference detected — return a fresh type variable as the mu-variable
            // for this recursive position. This gives recursive positions a proper type that
            // can be unified with the alias body rather than silently widening to Unknown.
            // Callers see a TypeVar(_tN) that unifies with the alias's expanded type.
            return Ok(state.fresh_type_var());
        }

        // Check arity
        if !alias.params.is_empty() {
            return Err(TypeError::new(
                format!(
                    "type alias '{}' expects {} type parameter(s), got 0",
                    name,
                    alias.params.len()
                ),
                span,
            ));
        }

        // Add to recursion guard
        recursion_guard.insert(name.to_string());

        // Expand the alias body (which is already a resolved Type)
        let result = expand_alias_body_guarded(
            &alias.body,
            env,
            state,
            ann_mapping,
            row_ann_mapping,
            recursion_guard,
            name,
            depth + 1,
            span,
        );

        // Remove from guard
        recursion_guard.remove(name);

        result
    } else {
        Err(TypeError::undefined_type(name, span))
    }
}

/// Returns true if `name` is a builtin type name that should NOT be treated as a
/// NominalVariant constructor even though it starts with an uppercase letter.
/// Used in `resolve_type_dict` and VarRef handling to distinguish `Int`, `Float`, etc.
/// from user-defined ADT constructor names like `Ok`, `None`, `Circle`.
fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name,
        "Int"
            | "Float"
            | "String"
            | "Bool"
            | "Number"
            | "Any"
            | "Seq"
            | "Handle"
            | "Null"
            | "Dict"
            | "Map"
            | "Record"
            | "Tuple"
            | "Fn"
            | "Never"
            | "Top"
            | "Unknown"
            | "Operator"
            | "Label"
            | "Str"
    )
}

pub(crate) fn resolve_type_name(
    name: &str,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &Option<&HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match name {
        "Int" => Ok(Type::Int),
        "Float" => Ok(Type::Float),
        "String" | "Str" => Ok(Type::Str),
        "Bool" => Ok(Type::Bool),
        "Number" => Ok(Type::Number),
        "Any" => Ok(Type::Top),
        "Proxy" => Ok(Type::Proxy),
        // BAS type names
        "Never" => Ok(Type::Never),
        "Top" => Ok(Type::Top),
        "Unknown" => Ok(Type::Unknown),
        "Operator" => Err(TypeError::new(
            "Operator is a kind, not a type — annotate a class type parameter as `f@Operator`, not a value expression",
            span,
        )),
        "Label" => {
            // Anonymous Label-kinded TypeVar (parallel to `@Operator` error above).
            // Create a fresh system-generated name like `_label_0`.
            // This is for when the label TypeVar is not referenced elsewhere (e.g., `get`/`get-or`).
            let fresh = format!("_label_{}", state.name_counter);
            state.name_counter = state.name_counter.saturating_add(1);
            state.levels.insert(fresh.clone(), state.level);
            state.kind_env.insert(fresh.clone(), crate::types::Kind::Label);
            Ok(Type::TypeVar(fresh, state.level))
        }
        "Seq" => Ok(Type::Seq(Box::new(Type::Unknown))),
        // Bare @Handle — no capability row argument. Resolves to Handle(Unknown),
        // which is the gradual "any handle" type. This is correct for unannotated
        // handle parameters where the caller doesn't know (or care about) the
        // capability row. Parameterized forms (`h@Handle@DirCap`, `[Handle DirCap]`,
        // `@Handle@DirCap`) resolve through resolve_annotated/resolve_annotation/
        // resolve_type_dict respectively and never reach this bare-name path.
        "Handle" => Ok(Type::Handle(Box::new(Type::Unknown))),
        "Null" => Ok(Type::Record(Row {
            fields: HashMap::new(),
        })),
        "Dict" => {
            // Empty record — represents "any dict" under BAS width subtyping.
            // Any concrete record is a subtype because all required fields (none) are present.
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }
        "Map" => {
            // Bare @Map → Map[Unknown: Unknown]
            Ok(Type::Map(Box::new(Type::Unknown), Box::new(Type::Unknown)))
        }
        "Record" => {
            // Bare @Record → open record (empty fields)
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }
        "Tuple" => {
            // Bare @Tuple → empty closed record (zero-element tuple = Null)
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }
        "Fn" => {
            // `@Fn` means "any callable".  Encode as a variadic function with zero
            // required parameters and Top return type — the top of the function
            // lattice.  A variadic function with 0 required params accepts calls of
            // any arity, so unification with a concrete function type (e.g.
            // `Fn(Int, Str) -> Bool`) succeeds: the concrete type satisfies the
            // "at least 0 params" contract.  Top as the return type is consistent
            // with any concrete return type via subtyping.
            //
            // There are currently 31 `@Fn` annotation sites in stdlib/prelude.llt.
            // These all annotate parameters that are genuinely function-valued but
            // whose precise signature is not statically known at the annotation site
            // (e.g. `pred@Fn`, `f@Fn`, `cmp@Fn`).  Using a precise `Function` type
            // here (rather than `Type::Unknown`) preserves callability enforcement:
            // a TypeAssert `[@Fn 42]` will correctly fail because `Int` is not a
            // subtype of `Function{...}`.
            Ok(Type::Function {
                params: vec![],
                ret: Box::new(Type::Top),
                variadic: true,
            })
        }
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
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
                    return Err(TypeError::new(
                        format!(
                            "annotation name '{name}' is already used as a row variable in this function; \
                             it cannot also be used as a type variable"
                        ),
                        span,
                    ));
                }

                // If we have an annotation mapping (within a function), check if this
                // annotation name has already been mapped to a fresh variable
                if let Some(ref mut mapping) = ann_mapping {
                    // Check if this annotation name already has a mapping
                    if let Some(existing_var) = mapping.get(name) {
                        // Already mapped: return the existing TypeVar with its current level
                        // from state.levels. DO NOT reset the level - unification may have
                        // lowered it, and level lowering must be monotone (Kiselyov 2013).
                        let current_level = *state
                            .levels
                            .get(existing_var)
                            .expect("invariant: annotation var registered in mapping must be in state.levels");
                        Ok(Type::TypeVar(existing_var.clone(), current_level))
                    } else {
                        // First time seeing this annotation: create fresh var and register level
                        let fresh = format!("_t{}", state.name_counter);
                        state.name_counter = state.name_counter.saturating_add(1);
                        state.levels.insert(fresh.clone(), state.level);
                        // Register source name for better T013 diagnostics
                        state.type_var_source_names.insert(fresh.clone(), name.to_string());
                        mapping.insert(name.to_string(), fresh.clone());
                        Ok(Type::TypeVar(fresh, state.level))
                    }
                } else {
                    // Outside of function scope: create a genuinely fresh type variable so
                    // two independent annotations like `[@a expr1]` and `[@a expr2]` at
                    // top-level do not share the same substitution variable and cause
                    // unintended unification.
                    //
                    // NOTE: we intentionally do NOT reuse the raw annotation name here.
                    // Using `name` directly means every occurrence of `@a` at the top
                    // level is the same TypeVar, causing unintended unification between
                    // unrelated dict entries that both happen to use `@a`.
                    Ok(state.fresh_type_var())
                }
            } else {
                // Uppercase type name — check for type alias
                if let Some(alias) = env.get_type_alias(name) {
                    // Check arity — bare alias name must have zero parameters
                    if !alias.params.is_empty() {
                        return Err(TypeError::new(
                            format!(
                                "type alias '{}' expects {} type parameter(s), got 0",
                                name,
                                alias.params.len()
                            ),
                            span,
                        ));
                    }

                    // Zero-parameter alias: return the body directly
                    // Recursive expansion happens during alias registration via resolve_type_name_with_guard
                    Ok(alias.body.clone())
                } else {
                    Err(TypeError::undefined_type(name, span))
                }
            }
        }
    }
}

/// Expand an alias body type, recursively expanding any nested type alias references.
/// Uses equi-recursive semantics (Amadio & Cardelli 1993) with a depth guard to prevent infinite unfolding.
/// The guard tracks aliases currently being expanded to detect cycles.
#[allow(clippy::too_many_arguments)] // Recursive helper with state threading
#[allow(clippy::only_used_in_recursion)] // env, state, ann_mapping, row_ann_mapping needed for recursive expansion
fn expand_alias_body_guarded(
    ty: &Type,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    alias_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    span: Span,
) -> Result<Type, TypeError> {
    // Depth guard (Amadio & Cardelli 1993)
    const MAX_ALIAS_DEPTH: usize = 256;
    if depth >= MAX_ALIAS_DEPTH {
        return Err(TypeError::new(
            format!(
                "recursive type alias '{}' exceeds maximum unfolding depth ({})",
                current_alias, MAX_ALIAS_DEPTH
            ),
            span,
        ));
    }

    // Add current alias to guard
    alias_guard.insert(current_alias.to_string());

    // Recursively expand the type structure
    let result = match ty {
        Type::Record(row) => {
            let mut new_fields = HashMap::new();
            for (k, v) in &row.fields {
                new_fields.insert(
                    k.clone(),
                    expand_alias_body_guarded(
                        v,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                        alias_guard,
                        current_alias,
                        depth,
                        span.clone(),
                    )?,
                );
            }
            Ok(Type::Record(Row { fields: new_fields }))
        }
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            let new_params = params
                .iter()
                .map(|(name, p_ty)| {
                    Ok((
                        name.clone(),
                        expand_alias_body_guarded(
                            p_ty,
                            env,
                            state,
                            ann_mapping,
                            row_ann_mapping,
                            alias_guard,
                            current_alias,
                            depth,
                            span.clone(),
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let new_ret = Box::new(expand_alias_body_guarded(
                ret,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span,
            )?);
            Ok(Type::Function {
                params: new_params,
                ret: new_ret,
                variadic: *variadic,
            })
        }
        Type::Seq(elem) => {
            let new_elem = Box::new(expand_alias_body_guarded(
                elem,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span,
            )?);
            Ok(Type::Seq(new_elem))
        }
        Type::Union(members) => {
            let new_members = members
                .iter()
                .map(|m| {
                    expand_alias_body_guarded(
                        m,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                        alias_guard,
                        current_alias,
                        depth,
                        span.clone(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Type::Union(new_members))
        }
        Type::Intersection(members) => {
            let new_members = members
                .iter()
                .map(|m| {
                    expand_alias_body_guarded(
                        m,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                        alias_guard,
                        current_alias,
                        depth,
                        span.clone(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Type::Intersection(new_members))
        }
        Type::Map(key, value) => {
            let new_key = Box::new(expand_alias_body_guarded(
                key,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span.clone(),
            )?);
            let new_value = Box::new(expand_alias_body_guarded(
                value,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span,
            )?);
            Ok(Type::Map(new_key, new_value))
        }
        // For all other types (primitives, type vars, etc.), return as-is
        // Note: Type alias references would be in type expressions, not in the resolved Type itself
        _ => Ok(ty.clone()),
    };

    // Remove current alias from guard after expansion
    alias_guard.remove(current_alias);

    result
}

/// Resolve a type expression with recursion guard for recursive type aliases.
/// This is the internal version used during alias registration.
#[allow(clippy::too_many_arguments)] // Internal helper for recursive type resolution
pub(crate) fn resolve_type_expr_with_guard(
    node: &Arc<SurfaceNode>,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
) -> Result<Type, TypeError> {
    const MAX_ALIAS_DEPTH: usize = 256;
    if depth >= MAX_ALIAS_DEPTH {
        return Err(TypeError::new(
            format!(
                "recursive type alias '{}' exceeds maximum unfolding depth ({})",
                current_alias, MAX_ALIAS_DEPTH
            ),
            node.span.clone(),
        ));
    }

    match &node.expr {
        SurfaceExpression::Str(s) => Ok(Type::StringLiteral(s.clone())),
        SurfaceExpression::VarRef { name, .. } => resolve_type_name_with_guard(
            name,
            env,
            node.span.clone(),
            state,
            ann_mapping,
            row_ann_mapping,
            recursion_guard,
            current_alias,
            depth,
        ),
        SurfaceExpression::Dict(entries) => resolve_type_dict_with_guard(
            entries,
            env,
            node.span.clone(),
            state,
            ann_mapping,
            row_ann_mapping,
            recursion_guard,
            current_alias,
            depth,
        ),
        _ => {
            // For all other expr types, delegate to normal resolve_type_expr.
            // Most expr types (literals, Annotated, Call) don't recursively reference type aliases,
            // so the guard isn't needed. If we encounter cases where nested aliases cause issues,
            // we can expand this match to handle them explicitly.
            resolve_type_expr(node, env, state, ann_mapping, row_ann_mapping)
        }
    }
}

/// Resolve a dict in type position with recursion guard.
#[allow(clippy::too_many_arguments)] // Internal helper for recursive type resolution
fn resolve_type_dict_with_guard(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
) -> Result<Type, TypeError> {
    // Handle type-stage keywords ([or ...], [all ...], [without ...]) with guard propagation.
    // Other dict forms (function types, parameterized aliases, record types, unions)
    // are delegated to the normal resolver which has more complex logic.
    let all_positional = entries.iter().all(|e| e.node.key.is_none());

    if all_positional && !entries.is_empty() {
        if let SurfaceExpression::VarRef { name: kw, .. } = &entries[0].node.value.expr {
            if kw == "or" {
                // [or T1 T2 ...] → Union([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeError::new(
                        "[or ...] requires at least one type argument",
                        span,
                    ));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr_with_guard(
                        &entry.node.value,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                        recursion_guard,
                        current_alias,
                        depth,
                    )?;
                    members.push(ty);
                }
                return Ok(Type::normalize_union(members));
            } else if kw == "all" {
                // [all T1 T2 ...] → Intersection([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeError::new(
                        "[all ...] requires at least one type argument",
                        span,
                    ));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr_with_guard(
                        &entry.node.value,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                        recursion_guard,
                        current_alias,
                        depth,
                    )?;
                    members.push(ty);
                }
                return Ok(Type::normalize_intersection(members));
            } else if kw == "without" {
                // [without A] → Negation(A)
                if entries.len() != 2 {
                    return Err(TypeError::new(
                        "[without A] requires exactly one type argument",
                        span,
                    ));
                }
                let inner = resolve_type_expr_with_guard(
                    &entries[1].node.value,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                    recursion_guard,
                    current_alias,
                    depth,
                )?;
                return Ok(Type::Negation(Box::new(inner)));
            }
        }
    }

    // Keyed record dict: `[value: Int  next: Node]` — the most common recursive alias body.
    // When all entries are keyed (or rest `...`), resolve each field value via
    // `resolve_type_expr_with_guard` so the recursion guard is respected for field types.
    // This is the critical path that was previously a dead code path: the old fallthrough
    // to `resolve_type_dict` called `resolve_type_name` (not `_with_guard`), so recursive
    // field references like `next: Node` silently returned the Unknown Pass-1 placeholder
    // instead of a fresh TypeVar.
    //
    // Positional-only forms (Fn types, [Seq T], [Map K V], [Tuple ...], parameterized aliases,
    // unions) are not keyed, so they fall through to `resolve_type_dict` as before.
    if !all_positional {
        // Reuse the same logic as the tail of resolve_type_dict but route field-value
        // resolution through resolve_type_expr_with_guard.
        let mut fields: HashMap<String, Type> = HashMap::new();
        for entry in entries {
            if let SurfaceExpression::Rest(_) = &entry.node.value.expr {
                // `...` rest notation: accepted for openness annotation, produces no field.
                continue;
            }
            let key = match &entry.node.key {
                Some(k) => match &k.expr {
                    SurfaceExpression::Str(s) => s.clone(),
                    _ => {
                        return Err(TypeError::new(
                            "type record keys must be bare words",
                            k.span.clone(),
                        ))
                    }
                },
                None => {
                    // Mixed keyed+positional dict — fall back to the full resolver.
                    return resolve_type_dict(
                        entries,
                        env,
                        span,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    );
                }
            };
            let ty = resolve_type_expr_with_guard(
                &entry.node.value,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
                recursion_guard,
                current_alias,
                depth,
            )?;
            fields.insert(key, ty);
        }

        // Mirror the multi-field intersection splitting from resolve_type_dict:
        // When two or more fields with no shared type variables are present, produce
        // Intersection of closed single-field Records (BAS open semantics).
        if fields.len() >= 2 {
            let mut all_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut has_shared = false;
            for ty in fields.values() {
                let mut field_type_vars = std::collections::HashSet::new();
                ty.collect_all_vars(&mut field_type_vars);
                for v in field_type_vars {
                    if !all_seen.insert(v) {
                        has_shared = true;
                        break;
                    }
                }
                if has_shared {
                    break;
                }
            }
            if !has_shared {
                let members: Vec<Type> = fields
                    .into_iter()
                    .map(|(k, v)| {
                        let mut member_fields = HashMap::new();
                        member_fields.insert(k, v);
                        Type::Record(Row {
                            fields: member_fields,
                        })
                    })
                    .collect();
                return Ok(Type::normalize_intersection(members));
            }
        }

        let ty = Type::Record(Row { fields });
        crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
        return Ok(ty);
    }

    // For remaining positional-only cases (function types, [Seq T], [Map K V], [Tuple ...],
    // parameterized alias applications, and multi-type unions), delegate to the normal
    // resolver which has the full dispatch logic for those forms.
    resolve_type_dict(entries, env, span, state, ann_mapping, row_ann_mapping)
}

pub(crate) fn resolve_type_expr(
    node: &Arc<SurfaceNode>,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match &node.expr {
        // String literals in type position → Type::StringLiteral (tag-only enum variants).
        // VarRef still goes to resolve_type_name for type alias lookup.
        SurfaceExpression::Str(s) => Ok(Type::StringLiteral(s.clone())),
        SurfaceExpression::VarRef { name, .. } => {
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
            match resolve_type_name(name, env, node.span.clone(), state, ann_mapping, &row_ref) {
                Ok(ty) => Ok(ty),
                Err(e) if crate::eval::is_constructor_name(name) => {
                    // Unknown uppercase name: treat as a zero-payload nominal variant constructor.
                    // This handles variant tags in type alias bodies such as `None` in
                    // `[type [Option a] [Some a] None]` where `None` has no payload.
                    let _ = e; // suppress the undefined-type error
                    Ok(Type::NominalVariant {
                        tag: name.clone(),
                        fields: Row {
                            fields: HashMap::new(),
                        },
                    })
                }
                Err(e) => Err(e),
            }
        }
        SurfaceExpression::Dict(entries) => resolve_type_dict(
            entries,
            env,
            node.span.clone(),
            state,
            ann_mapping,
            row_ann_mapping,
        ),
        SurfaceExpression::Annotated { name, annotation } => {
            if name == "Fn" {
                resolve_fn_type(
                    &annotation.node,
                    env,
                    annotation.span.clone(),
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            } else {
                // For all other parameterized type annotations in type-expression position
                // (e.g., `Handle@DirCap`, `Seq@Int`, `Map@[key: Str value: Int]` inline),
                // reconstruct the `Annotation::Annotated(name, inner)` and dispatch through
                // `resolve_annotation` which handles `"Handle"`, `"Seq"`, `"Map"`, etc.
                //
                // Previously this called `resolve_annotation(&annotation.node, ...)` which
                // dropped `name` entirely and resolved only the inner annotation — losing
                // the Handle wrapper for `Handle@DirCap`, Seq wrapper for `Seq@Int`, etc.
                let full_ann =
                    Annotation::Annotated(name.clone(), Box::new(annotation.node.clone()));
                resolve_annotation(
                    &full_ann,
                    env,
                    node.span.clone(),
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            }
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
            if let SurfaceExpression::Annotated { name, annotation } = &func.expr {
                if name == "Fn" {
                    // Fn@RetType [Params] in new syntax: resolve return type from annotation,
                    // then resolve each arg as a parameter type. For zero params, args is empty.
                    let ret = resolve_annotation_as_type(
                        &annotation.node,
                        env,
                        annotation.span.clone(),
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
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
                                            SurfaceExpression::Str(s) => Some(s.clone()),
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    };
                                    let param_ty = resolve_type_expr(
                                        &entry.node.value,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )?;
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
                                let param_ty = resolve_type_expr(
                                    inner_func,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                params.push((None, param_ty));
                                for a in inner_args.iter() {
                                    let param_ty = resolve_type_expr(
                                        a,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )?;
                                    params.push((None, param_ty));
                                }
                            }
                            _ => {
                                // Single param that's not a Dict
                                let param_ty = resolve_type_expr(
                                    param_list,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                params.push((None, param_ty));
                            }
                        }
                    }
                    if args.len() > 1 {
                        return Err(TypeError::new(
                            format!(
                                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                                1 + args.len()
                            ),
                            node.span.clone(),
                        ));
                    }
                    return Ok(Type::Function {
                        params,
                        ret: Box::new(ret),
                        variadic: false,
                    });
                }
            }

            // Type-stage keywords in implied-call position: [or T1 T2], [all T1 T2], [without T].
            //
            // These parse as SurfaceExpression::Call { func: VarRef(kw), args: [...], implied: true }
            // because the parser sees a bare identifier in head position followed by arguments.
            //
            // [or T1 T2 ...]  → Type::normalize_union([T1, T2, ...])
            // [all T1 T2 ...] → Type::normalize_intersection([T1, T2, ...])
            // [without T]     → Type::Negation(T)
            if let SurfaceExpression::VarRef { name: kw, .. } = &func.expr {
                if kw == "or" {
                    // args contains the type arguments; func ("or") is the head, not a type.
                    if args.is_empty() {
                        return Err(TypeError::new(
                            "[or ...] requires at least one type argument",
                            node.span.clone(),
                        ));
                    }
                    let mut members = Vec::new();
                    for arg in args.iter() {
                        let ty = resolve_type_expr(arg, env, state, ann_mapping, row_ann_mapping)?;
                        members.push(ty);
                    }
                    return Ok(Type::normalize_union(members));
                } else if kw == "all" {
                    // args contains the type arguments; func ("all") is the head, not a type.
                    if args.is_empty() {
                        return Err(TypeError::new(
                            "[all ...] requires at least one type argument",
                            node.span.clone(),
                        ));
                    }
                    let mut members = Vec::new();
                    for arg in args.iter() {
                        let ty = resolve_type_expr(arg, env, state, ann_mapping, row_ann_mapping)?;
                        members.push(ty);
                    }
                    return Ok(Type::normalize_intersection(members));
                } else if kw == "without" {
                    if args.len() != 1 {
                        return Err(TypeError::new(
                            "[without A] requires exactly one type argument",
                            node.span.clone(),
                        ));
                    }
                    let inner =
                        resolve_type_expr(&args[0], env, state, ann_mapping, row_ann_mapping)?;
                    return Ok(Type::Negation(Box::new(inner)));
                }
            }

            // Built-in type constructor application in implied-call position.
            // Check BEFORE parameterized alias lookup so built-in constructors have priority.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                match name.as_str() {
                    "Tuple" => {
                        // [Tuple T0 T1 T2 ...] → closed record {0: T0, 1: T1, 2: T2, ...}
                        // Encodes tuple types as closed records with integer-string field names.
                        // args[0..] are the element types (func is "Tuple", args are the types).
                        let mut fields = HashMap::new();
                        for (idx, arg) in args.iter().enumerate() {
                            let elem_ty =
                                resolve_type_expr(arg, env, state, ann_mapping, row_ann_mapping)?;
                            fields.insert(idx.to_string(), elem_ty);
                        }
                        return Ok(Type::Record(Row { fields }));
                    }
                    "Seq" => {
                        if args.len() == 1 {
                            let elem_ty = resolve_type_expr(
                                &args[0],
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            return Ok(Type::Seq(Box::new(elem_ty)));
                        } else {
                            return Err(TypeError::new(
                                "Seq requires 1 type argument",
                                node.span.clone(),
                            ));
                        }
                    }
                    "Map" => {
                        if args.len() == 2 {
                            let key_ty = resolve_type_expr(
                                &args[0],
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            let value_ty = resolve_type_expr(
                                &args[1],
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            return Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)));
                        } else if args.len() == 1 {
                            // Single-arg form: [Map Int] → Map(fresh_key, Int)
                            // Use a fresh TypeVar for the key so callers can unify against
                            // concrete key types instead of being stuck with Unknown.
                            let value_ty = resolve_type_expr(
                                &args[0],
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            return Ok(Type::Map(
                                Box::new(state.fresh_type_var()),
                                Box::new(value_ty),
                            ));
                        } else {
                            return Err(TypeError::new(
                                "Map requires 1 or 2 type arguments",
                                node.span.clone(),
                            ));
                        }
                    }
                    "Handle" => {
                        // [Handle CapType] → Handle(CapType) in implied-call position.
                        //
                        // When `[Handle DirCap]` is parsed as an implied call rather than a Dict
                        // (which happens when the value appears inside an annotation's value slot,
                        // e.g. `fn@[return: [Handle DirCap]]`), handle it here so it produces
                        // Handle(DirCap) rather than failing with "alias expects 0 params, got 1".
                        //
                        // Examples:
                        //   [Handle DirCap]  → Handle(DirCap)
                        //   [Handle NetCap]  → Handle(NetCap)
                        //   [Handle Unknown] → Handle(Unknown)  (gradual)
                        if args.len() == 1 {
                            let cap_ty = resolve_type_expr(
                                &args[0],
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            return Ok(Type::Handle(Box::new(cap_ty)));
                        } else if args.is_empty() {
                            // Bare [Handle] in call position — gradual handle
                            return Ok(Type::Handle(Box::new(Type::Unknown)));
                        } else {
                            return Err(TypeError::new(
                                "Handle requires 0 or 1 type argument (the capability row)",
                                node.span.clone(),
                            ));
                        }
                    }
                    _ => {}
                }
            }

            // Check if this is a parameterized type alias application: [AliasName Arg1 Arg2]
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if let Some(alias) = env.get_type_alias(name) {
                    // Resolve all type arguments
                    let mut type_args = Vec::new();
                    for arg in args {
                        type_args.push(resolve_type_expr(
                            arg,
                            env,
                            state,
                            ann_mapping,
                            row_ann_mapping,
                        )?);
                    }

                    // Check arity
                    if type_args.len() != alias.params.len() {
                        return Err(TypeError::new(
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
                    return instantiate_type_alias(alias, &type_args, state);
                }
            }

            // Nominal constructor: [ConstructorName field1: T1 field2: T2 ...]
            // Check if func is an uppercase VarRef (nominal constructor name).
            // Builtin type names (Int, Float, etc.) must NOT be treated as NominalVariant.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if crate::eval::is_constructor_name(name) && !is_builtin_type_name(name) {
                    // This is a nominal variant constructor with named fields.
                    // args is empty, named_args contains the field types.
                    // We need to resolve each field type and build a NominalVariant.
                    if !named_args.is_empty() {
                        let mut fields_map = HashMap::new();
                        for named_arg in named_args {
                            let field_ty = resolve_type_expr(
                                &named_arg.node.value,
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            fields_map.insert(named_arg.node.name.clone(), field_ty);
                        }
                        return Ok(Type::NominalVariant {
                            tag: name.clone(),
                            fields: Row { fields: fields_map },
                        });
                    } else if args.len() == 1 {
                        // Single positional payload: [Some a] → NominalVariant("Some", { "0": a })
                        // Use integer string key "0" for the single positional payload field.
                        let payload_ty =
                            resolve_type_expr(&args[0], env, state, ann_mapping, row_ann_mapping)?;
                        let mut fields_map = HashMap::new();
                        fields_map.insert("0".to_string(), payload_ty);
                        return Ok(Type::NominalVariant {
                            tag: name.clone(),
                            fields: Row { fields: fields_map },
                        });
                    } else if args.is_empty() {
                        // Unit constructor: [None] → NominalVariant("None", {})
                        return Ok(Type::NominalVariant {
                            tag: name.clone(),
                            fields: Row {
                                fields: HashMap::new(),
                            },
                        });
                    } else {
                        return Err(TypeError::new(
                            format!(
                                "nominal constructor {} requires either 0 args, 1 arg, or named args",
                                name
                            ),
                            node.span.clone(),
                        ));
                    }
                }
            }

            // Lowercase VarRef in implied-call head position with args: treat as Union type.
            //
            // Pattern: [a T1 T2 ...] where `a` is lowercase (a type variable).
            // Interpretation: Union([TypeVar(a), T1, T2, ...])
            //
            // This handles prelude annotations like `[return: [a Null]]` in:
            //   cond: [fn@[return: [a Null] doc: "..."] ...]
            //   when: [fn@[return: [a Null] doc: "..."] ...]
            //   unless: [fn@[return: [a Null] doc: "..."] ...]
            //
            // In these annotations, `a` is a type variable and `Null` is the empty record.
            // The parser sees `[a Null]` as an implied call `Call(VarRef("a"), [VarRef("Null")])`
            // because `a` in head position without `:` or `@` is treated as a function name.
            // The intended meaning is `Union([TypeVar(a), Null])` which we recover here.
            if let SurfaceExpression::VarRef {
                name: func_name, ..
            } = &func.expr
            {
                if func_name.starts_with(|c: char| c.is_lowercase()) && !args.is_empty() {
                    // Treat func as a type variable and resolve all args as type members.
                    // Form: Union([TypeVar(func_name), arg0_ty, arg1_ty, ...])
                    let head_ty = resolve_type_name(
                        func_name,
                        env,
                        func.span.clone(),
                        state,
                        ann_mapping,
                        &row_ann_mapping.as_ref().map(|m| &**m),
                    )?;
                    let mut members = vec![head_ty];
                    for arg in args.iter() {
                        let member_ty =
                            resolve_type_expr(arg, env, state, ann_mapping, row_ann_mapping)?;
                        members.push(member_ty);
                    }
                    return Ok(Type::normalize_union(members));
                }
            }

            Err(TypeError::new(
                format!("invalid type expression in annotation: {:?}", node.expr),
                node.span.clone(),
            ))
        }
        _ => Err(TypeError::new(
            format!("invalid type expression in annotation: {:?}", node.expr),
            node.span.clone(),
        )),
    }
}

pub(crate) fn resolve_type_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if let Some(fn_type) = try_resolve_fn_type_expr(
        entries,
        env,
        span.clone(),
        state,
        ann_mapping,
        row_ann_mapping,
    )? {
        return Ok(fn_type);
    }

    // Built-in type constructor application: [Seq Int], [Map String Int]
    // Check BEFORE parameterized alias lookup so built-in constructors have priority.
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    match name.as_str() {
                        "Seq" => {
                            if entries.len() == 2 {
                                let elem_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                return Ok(Type::Seq(Box::new(elem_ty)));
                            } else {
                                return Err(TypeError::new("Seq requires 1 type argument", span));
                            }
                        }
                        "Tuple" => {
                            // [Tuple T0 T1 T2 ...] → closed record {0: T0, 1: T1, 2: T2, ...}
                            // Encodes tuple types as closed records with integer-string field names.
                            // Matches the evaluation model: tuples are dicts with integer keys.
                            // Zero-element tuple → empty closed record (Null type).
                            // entries[0] is the "Tuple" keyword; entries[1..] are the element types.
                            let mut fields = HashMap::new();
                            for (idx, entry) in entries[1..].iter().enumerate() {
                                let elem_ty = resolve_type_expr(
                                    &entry.node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                fields.insert(idx.to_string(), elem_ty);
                            }
                            return Ok(Type::Record(Row { fields }));
                        }
                        "Map" => {
                            if entries.len() == 3 {
                                let key_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                let value_ty = resolve_type_expr(
                                    &entries[2].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                return Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)));
                            } else if entries.len() == 2 {
                                // Single-arg form: [Map Int] → Map(fresh_key, Int)
                                // Use a fresh TypeVar for the key so callers can unify against
                                // concrete key types instead of being stuck with Unknown.
                                let value_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                return Ok(Type::Map(
                                    Box::new(state.fresh_type_var()),
                                    Box::new(value_ty),
                                ));
                            } else {
                                return Err(TypeError::new(
                                    "Map requires 1 or 2 type arguments",
                                    span,
                                ));
                            }
                        }
                        "Handle" => {
                            // [Handle CapType] → Handle(CapType)
                            //
                            // Parameterized handle type in dict/type-expression position.
                            // The capability row argument is the second positional entry.
                            //
                            // Examples:
                            //   [Handle DirCap]     → Handle(DirCap)
                            //   [Handle NetCap]     → Handle(NetCap)
                            //   [Handle Unknown]    → Handle(Unknown)  (gradual handle)
                            //   [Handle]            → Handle(Unknown)  (bare — no cap_row)
                            //
                            // This mirrors the `@Handle@CapType` subscript form handled in
                            // `resolve_annotation` and `resolve_annotated`.
                            if entries.len() == 2 {
                                let cap_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                return Ok(Type::Handle(Box::new(cap_ty)));
                            } else if entries.len() == 1 {
                                // Bare [Handle] — gradual handle accepting any capability.
                                return Ok(Type::Handle(Box::new(Type::Unknown)));
                            } else {
                                return Err(TypeError::new(
                                    "Handle requires 0 or 1 type argument (the capability row)",
                                    span,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Parameterized type alias application: [AliasName Arg1 Arg2 ...]
    // When the first entry is auto-indexed and refers to a parameterized type alias,
    // treat remaining auto-indexed entries as type arguments.
    // This MUST run before union detection so [Result Int] resolves as alias
    // application, not Union(Result, Int).
    if entries.len() >= 2 {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    if let Some(alias) = env.get_type_alias(name) {
                        if !alias.params.is_empty() {
                            let mut type_args = Vec::new();
                            for entry in &entries[1..] {
                                if entry.node.key.is_some() {
                                    return Err(TypeError::new(
                                        format!(
                                            "unexpected keyed entry in type alias application '{}'",
                                            name
                                        ),
                                        entry.span.clone(),
                                    ));
                                }
                                type_args.push(resolve_type_expr(
                                    &entry.node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?);
                            }
                            if type_args.len() != alias.params.len() {
                                return Err(TypeError::new(
                                    format!(
                                        "type alias '{}' expects {} type parameter(s), got {}",
                                        name,
                                        alias.params.len(),
                                        type_args.len()
                                    ),
                                    span,
                                ));
                            }
                            return instantiate_type_alias(alias, &type_args, state);
                        }
                    }
                }
            }
        }
    }

    // BAS annotation keywords: [or A B] → Union(A, B), [all A B] → Intersection(A, B),
    // [without A] → Negation(A).
    //
    // These correspond to type-stage function names that map directly to Type variants
    // without needing runtime eval machinery. They must be checked BEFORE the general
    // all-positional union path so `[all Int Str]` dispatches to Intersection (not Union)
    // and `[or Int Str]` dispatches to Union via this explicit path (not the fallthrough
    // union path which would error on "undefined type 'or'").
    //
    // [or A B C ...]  → Type::normalize_union([A, B, C, ...])
    // [all A B C ...] → Type::normalize_intersection([A, B, C, ...])
    // [without A]     → Type::Negation(A)
    let all_positional = entries.iter().all(|e| e.node.key.is_none());
    if all_positional && !entries.is_empty() {
        if let SurfaceExpression::VarRef { name: kw, .. } = &entries[0].node.value.expr {
            if kw == "or" {
                // [or T1 T2 ...] → Union([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeError::new(
                        "[or ...] requires at least one type argument",
                        span,
                    ));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr(
                        &entry.node.value,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
                    members.push(ty);
                }
                return Ok(Type::normalize_union(members));
            } else if kw == "all" {
                // [all T1 T2 ...] → Intersection([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeError::new(
                        "[all ...] requires at least one type argument",
                        span,
                    ));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr(
                        &entry.node.value,
                        env,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
                    members.push(ty);
                }
                return Ok(Type::normalize_intersection(members));
            } else if kw == "without" {
                // [without A] → Negation(A)
                if entries.len() != 2 {
                    return Err(TypeError::new(
                        "[without A] requires exactly one type argument",
                        span,
                    ));
                }
                let inner = resolve_type_expr(
                    &entries[1].node.value,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )?;
                return Ok(Type::Negation(Box::new(inner)));
            }
        }
    }

    // Type constructor application: [f a] where f is Operator-kinded (hkt-kind-inference Task 2)
    // Must check BEFORE union type path so `[m Int]` where m is Operator-kinded becomes
    // `Type::App(Operator("m"), Int)`, not `Union(Operator("m"), Int)`.
    if all_positional && entries.len() == 2 {
        if let SurfaceExpression::VarRef { name: f_name, .. } = &entries[0].node.value.expr {
            // Check if f is Operator-kinded in kind_env
            if let Some(Kind::Operator) = state.kind_env.get(f_name) {
                let f_type = Type::Operator(f_name.clone());
                let a_type = resolve_type_expr(
                    &entries[1].node.value,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )?;

                // Rank-1 restriction (hkt-kind-inference Task 3): reject App(Operator, Operator)
                if let Type::Operator(op_name) = &a_type {
                    return Err(TypeError::new(
                        format!(
                            "kind mismatch: expected `*`, got `* → *` — type constructor `{}` cannot be applied to another type constructor `{}`; use a concrete type instead",
                            f_name, op_name
                        ),
                        span,
                    ));
                }

                return Ok(Type::App(Box::new(f_type), Box::new(a_type)));
            }
        }
    }

    // Nominal variant constructor: [Constructor payload-type] or [Constructor field: Type ...]
    // Matches either form:
    // - Pure positional with uppercase first entry (e.g., [Ok a], [None]):
    //   First entry is constructor tag, optional second entry is payload type
    // - Mixed positional+keyed with uppercase first entry (e.g., [MyOk n: Int]):
    //   First positional is constructor tag, keyed entries are named fields
    //
    // This must be checked BEFORE the multi-entry union path below so that individual
    // constructor expressions like [Ok a] resolve to NominalVariant, not Union(Ok, a).
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            // Check if first entry is positional (auto-indexed)
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name: tag, .. } = &first.node.value.expr {
                    // Check if tag is uppercase (constructor name).
                    // BUT: builtin type names (Int, Float, String, Bool, Number, etc.) also
                    // start with uppercase and must NOT be treated as NominalVariant.
                    // Resolve builtin type names through resolve_type_name first.
                    let is_builtin_type = is_builtin_type_name(tag);
                    if is_builtin_type && entries.len() == 1 && first.node.key.is_none() {
                        // Single positional entry that is a builtin type name: [Int] → Type::Int.
                        // This handles annotations like @[Int] which should resolve to Int,
                        // not to NominalVariant { tag: "Int" }.
                        let row_ref: Option<&HashMap<String, String>> =
                            row_ann_mapping.as_ref().map(|m| &**m);
                        return resolve_type_name(tag, env, span, state, ann_mapping, &row_ref);
                    }
                    if crate::eval::is_constructor_name(tag) && !is_builtin_type {
                        // Case 1: Pure positional — [Constructor] or [Constructor PayloadType]
                        let all_remaining_positional =
                            entries[1..].iter().all(|e| e.node.key.is_none());
                        if all_remaining_positional {
                            if entries.len() == 1 {
                                // Unit constructor: [None]
                                return Ok(Type::NominalVariant {
                                    tag: tag.clone(),
                                    fields: Row {
                                        fields: HashMap::new(),
                                    },
                                });
                            } else if entries.len() == 2 {
                                // Single-payload constructor: [Ok a]
                                let payload_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                // Unnamed payload: create record with single field "0"
                                let mut fields = HashMap::new();
                                fields.insert("0".to_string(), payload_ty);
                                return Ok(Type::NominalVariant {
                                    tag: tag.clone(),
                                    fields: Row { fields },
                                });
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
                            let mut variant_fields = HashMap::new();
                            for field_entry in &entries[1..] {
                                match &field_entry.node.key {
                                    Some(k) => {
                                        let field_name = match &k.expr {
                                            SurfaceExpression::Str(s) => s.clone(),
                                            _ => return Err(TypeError::new(
                                                "nominal variant field names must be bare words",
                                                k.span.clone(),
                                            )),
                                        };
                                        let field_ty = resolve_type_expr(
                                            &field_entry.node.value,
                                            env,
                                            state,
                                            ann_mapping,
                                            row_ann_mapping,
                                        )?;
                                        variant_fields.insert(field_name, field_ty);
                                    }
                                    None => {
                                        return Err(TypeError::new(
                                            "nominal variant constructor with named fields requires all fields after the constructor tag to be keyed (field: Type)",
                                            field_entry.span.clone(),
                                        ));
                                    }
                                }
                            }
                            return Ok(Type::NominalVariant {
                                tag: tag.clone(),
                                fields: Row {
                                    fields: variant_fields,
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    // Single positional entry that is NOT a VarRef: delegate to resolve_type_expr.
    // This handles complex type expressions like [all [x: Int ...] [y: String ...]], [or A B],
    // and [Seq Int] when they appear as the sole positional entry in a type dict.
    // resolve_type_expr handles SurfaceExpression::Call (implied calls) and
    // SurfaceExpression::Dict forms which are not handled by the VarRef-specific paths above.
    if all_positional && entries.len() == 1 {
        if let Some(first) = entries.first() {
            if first.node.key.is_none()
                && !matches!(&first.node.value.expr, SurfaceExpression::VarRef { .. })
            {
                return resolve_type_expr(
                    &first.node.value,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                );
            }
        }
    }

    // Multi-entry union type from `[type T1 T2 ...]` declarations.
    // When ALL entries are auto-indexed (no keys) and there are 2+ entries,
    // this is a union of type expressions (not a record type).
    // Single auto-indexed entry falls through to existing handling.
    // Note: simplify_type is intentionally NOT called here — annotation-declared
    // union types (e.g., ADT type aliases [type [Ok a] [Err String]]) must be
    // preserved exactly as declared and not collapsed by S-RcdTop/S-ClsBot rules.
    if all_positional && entries.len() >= 2 {
        let mut members = Vec::new();
        for entry in entries {
            let member_ty =
                resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
            members.push(member_ty);
        }
        return Ok(Type::normalize_union(members));
    }

    // Mixed positional + metadata-keyed annotation: [Int String default: 0]
    // Positional entries are union type members; metadata keys (default, repr, doc) are ignored.
    // E.g.: @[Int String default: 0] → Union(Int, Str) (default: 0 is runtime metadata, not a type).
    {
        const METADATA_KEYS: &[&str] = &["default", "repr", "doc"];
        let positional_entries: Vec<_> = entries.iter().filter(|e| e.node.key.is_none()).collect();
        let has_only_metadata_non_positional = entries.iter().all(|e| {
            e.node.key.is_none()
                || e.node.key.as_ref().is_some_and(|k| {
                    if let SurfaceExpression::Str(s) = &k.expr {
                        METADATA_KEYS.contains(&s.as_str())
                    } else {
                        false
                    }
                })
        });
        if !positional_entries.is_empty() && has_only_metadata_non_positional && !all_positional
        // only if there are SOME keyed entries (otherwise all_positional path handles it)
        {
            let mut members = Vec::new();
            for entry in &positional_entries {
                let member_ty =
                    resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
                members.push(member_ty);
            }
            return Ok(Type::normalize_union(members));
        }
    }

    let mut fields: HashMap<String, Type> = HashMap::new();
    let mut has_rest = false; // tracks if `...` is present (BAS: openness via width subtyping)
    for entry in entries {
        if let SurfaceExpression::Rest(_name) = &entry.node.value.expr {
            // BAS: `...` annotations express user intent for openness; under BAS width
            // subtyping all records are closed — is_subtype handles extra fields.
            has_rest = true;
            continue;
        }
        let key = match &entry.node.key {
            Some(k) => match &k.expr {
                SurfaceExpression::Str(s) => s.clone(),
                _ => {
                    return Err(TypeError::new(
                        "type record keys must be bare words",
                        k.span.clone(),
                    ))
                }
            },
            None => {
                return Err(TypeError::new(
                    "auto-indexed entries not supported in type expressions",
                    entry.span.clone(),
                ))
            }
        };
        let ty = resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
        fields.insert(key, ty);
    }

    // Multi-field record annotation → intersection of closed single-field records.
    //
    // `@[x: Int  y: String]` → `Intersection([{x: Int}, {y: String}])`
    //
    // Under BAS open semantics (Step 1 of RowVar removal), each member is a CLOSED record
    // (RowTail::Empty). Openness is expressed via conjunction elimination in is_subtype:
    // `{x:1, y:"hello"} <: {x:Int}` succeeds because width subtyping allows extra fields
    // in the sub-record (BAS Step 2 of is_subtype). No RowVar needed for openness.
    //
    // Single-field annotations (`@[name: String]`) fall through to line ~1494 with
    // tail = Empty (no `...` written). Under BAS, `{name: "Alice", age: 30} <: {name: Str}`
    // now succeeds via width subtyping, so single-field annotations are open by default.
    //
    // Annotations with a rest entry (`@[x: Int ...]`) bypass this path.
    // Under BAS, `...` is accepted as annotation syntax but produces the same closed
    // Record — BAS width subtyping handles openness structurally. No RowVar tail exists.
    //
    // SHARED TYPE VARIABLE GUARD: If any TypeVar name appears in more than one field,
    // fall back to the closed Record. Splitting into single-field members would cause
    // each member to independently bind the shared TypeVar to a different concrete value
    // during unification, producing spurious "cannot unify X with Y" errors.
    // Example: `[type [a] [first: a  second: a]]` — both fields share `a`; if split into
    // `{first: a}` and `{second: a}`, unifying with `{first: 1, second: 2}` first binds
    // `a = 1` then tries to unify `a` (= 1) with 2 → error.
    if fields.len() >= 2 && !has_rest {
        // Check for shared TypeVar names across field types
        let mut all_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut has_shared = false;
        for ty in fields.values() {
            let mut field_type_vars = std::collections::HashSet::new();
            ty.collect_all_vars(&mut field_type_vars);
            for v in field_type_vars {
                if !all_seen.insert(v) {
                    has_shared = true;
                    break;
                }
            }
            if has_shared {
                break;
            }
        }

        if !has_shared {
            let members: Vec<Type> = fields
                .into_iter()
                .map(|(k, v)| {
                    // Under BAS open semantics, structural annotations are open by default
                    // via conjunction elimination — a RowVar is no longer needed to express
                    // openness. Each single-field member uses a closed (Empty) row tail.
                    // Width subtyping in is_subtype (BAS Step 2) allows closed records with
                    // extra fields to satisfy these closed single-field members:
                    //   {name: "Alice", age: 30} <: {name: String} (closed)
                    // because {name: String} is structurally "has at least name: String"
                    // under BAS conjunction-elimination semantics.
                    let mut member_fields = HashMap::new();
                    member_fields.insert(k, v);
                    Type::Record(Row {
                        fields: member_fields,
                    })
                })
                .collect();
            return Ok(Type::normalize_intersection(members));
        }
    }

    let ty = Type::Record(Row { fields });
    crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
    Ok(ty)
}

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
fn try_resolve_fn_type_expr(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Option<Type>, TypeError> {
    let first = match entries.first() {
        Some(e) if e.node.key.is_none() => e,
        _ => return Ok(None),
    };

    let (ann_node, ann_span) = match &first.node.value.expr {
        SurfaceExpression::Annotated { name, annotation } if name == "Fn" => {
            (&annotation.node, annotation.span.clone())
        }
        _ => return Ok(None),
    };

    if entries.len() != 2 {
        return Err(TypeError::new(
            format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
        ));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(TypeError::new(
            "function type parameter list must be auto-indexed",
            second.span.clone(),
        ));
    }

    let ret =
        resolve_annotation_as_type(ann_node, env, ann_span, state, ann_mapping, row_ann_mapping)?;

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
                        SurfaceExpression::Str(s) => Some(s.clone()),
                        _ => None,
                    }
                } else {
                    None
                };
                let param_ty =
                    resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
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
            let param_ty = resolve_type_expr(func, env, state, ann_mapping, row_ann_mapping)?;
            params.push((None, param_ty));
            for arg in args.iter() {
                let param_ty = resolve_type_expr(arg, env, state, ann_mapping, row_ann_mapping)?;
                params.push((None, param_ty));
            }
        }
        _ => {
            return Err(TypeError::new(
                "function type parameter list must be a bracket expression",
                second.node.value.span.clone(),
            ))
        }
    }

    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
        variadic: false,
    }))
}
