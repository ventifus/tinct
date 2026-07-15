//! Path-sensitive narrowing, pattern binding extraction, and overlap checking.
//!
//! This module contains the subsystem responsible for:
//! - Extracting type narrowing constraints from conditional expressions (`if`, match guards)
//! - Applying those constraints to fork the type environment for true/false branches
//! - Collecting variable bindings introduced by match patterns with inferred types
//! - Instance pattern type extraction and functional-dependency parameter index resolution
//! - Pattern overlap / type unification probes (side-effect-free)

use std::sync::{Arc, RwLock};

use crate::ast::{Annotation, Pattern, Span, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::types::{InferState, Row, Type, TypeError, TypeScheme};

/// Narrowing constraints extracted from conditional expressions.
/// Each constraint refines the type of a variable in the true branch of an `if`.
#[derive(Debug, Clone)]
pub(crate) enum Narrowing {
    /// `[= var literal]` narrows `var` to the literal type.
    EqLiteral { var: String, ty: Type },
    /// `[= [type-of var] "TypeName"]` narrows `var` to the named type.
    TypeOf { var: String, ty: Type },
    /// `[has? var "key"]` narrows `var` to a record with at least that key.
    HasKey { var: String, key: String },
}

/// Extract narrowing constraints from a condition expression (SurfaceNode version).
/// Returns an empty vec for unrecognized patterns.
pub(crate) fn extract_narrowings(cond: &Arc<SurfaceNode>) -> Vec<Narrowing> {
    match &cond.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } if named_args.is_empty() => {
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                match name.as_str() {
                    // Pattern: [= x literal] or [= literal x]
                    "=" if args.len() == 2 => {
                        // Try both operand orderings
                        if let Some(narrowing) = try_eq_literal(&args[0], &args[1]) {
                            return vec![narrowing];
                        }
                        if let Some(narrowing) = try_eq_literal(&args[1], &args[0]) {
                            return vec![narrowing];
                        }
                        // Try type-of pattern: [= [type-of x] "TypeName"]
                        if let Some(narrowing) = try_type_of(&args[0], &args[1]) {
                            return vec![narrowing];
                        }
                        if let Some(narrowing) = try_type_of(&args[1], &args[0]) {
                            return vec![narrowing];
                        }
                    }
                    // Pattern: [has? x "key"]
                    "has?" if args.len() == 2 => {
                        if let (
                            SurfaceExpression::VarRef { name: var_name, .. },
                            SurfaceExpression::StringLiteral { content: key, .. },
                        ) = (&args[0].expr, &args[1].expr)
                        {
                            return vec![Narrowing::HasKey {
                                var: var_name.clone(),
                                key: key.clone(),
                            }];
                        }
                    }
                    // Pattern: [and cond1 cond2 ...]
                    "and" => {
                        let mut narrowings = Vec::new();
                        for arg in args {
                            narrowings.extend(extract_narrowings(arg));
                        }
                        return narrowings;
                    }
                    // Pattern: [int? x], [str? x], [dict? x], [bool? x], [float? x],
                    // [fn? x], [null? x], [seq? x], [num? x]
                    "int?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Int,
                            }];
                        }
                    }
                    "str?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Str,
                            }];
                        }
                    }
                    "dict?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // dict? narrows to open record with no fields
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Dict(Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                }),
                            }];
                        }
                    }
                    "bool?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::TyCon("Boolean".to_string()),
                            }];
                        }
                    }
                    "float?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Float,
                            }];
                        }
                    }
                    "fn?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Function {
                                    params: vec![],
                                    ret: Box::new(Type::Unknown),
                                    variadic: true,
                                    required_count: 0,
                                },
                            }];
                        }
                    }
                    "null?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // null? narrows to empty closed record
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Dict(Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                }),
                            }];
                        }
                    }
                    "seq?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // seq? narrows to App(TyCon("Seq"), Unknown)
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::App(
                                    Box::new(Type::TyCon("Seq".to_string())),
                                    Box::new(Type::Unknown),
                                ),
                            }];
                        }
                    }
                    "num?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // num? narrows to Int | Float
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::normalize_union(vec![Type::Int, Type::Float]),
                            }];
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Try to extract an equality-literal narrowing from `[= var literal]`.
pub(crate) fn try_eq_literal(
    left: &Arc<SurfaceNode>,
    right: &Arc<SurfaceNode>,
) -> Option<Narrowing> {
    if let SurfaceExpression::VarRef { name, .. } = &left.expr {
        match &right.expr {
            SurfaceExpression::Int(n) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::IntLiteral(*n),
            }),
            SurfaceExpression::StringLiteral { content: s, .. } => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::StringLiteral(s.clone()),
            }),
            // Bool: no native boolean type — literals are booleans via TyCon
            // Skip bool literal narrowing for now
            SurfaceExpression::VarRef { name: ref n, .. } if n == "true" || n == "false" => {
                Some(Narrowing::EqLiteral {
                    var: name.clone(),
                    ty: Type::TyCon("Boolean".to_string()),
                })
            }
            _ => None,
        }
    } else {
        None
    }
}

/// Try to extract a type-of narrowing from `[= [type-of var] "TypeName"]`.
pub(crate) fn try_type_of(left: &Arc<SurfaceNode>, right: &Arc<SurfaceNode>) -> Option<Narrowing> {
    // Left side must be [type-of var]
    if let SurfaceExpression::Call {
        func,
        args,
        named_args,
        ..
    } = &left.expr
    {
        if named_args.is_empty() && args.len() == 1 {
            if let SurfaceExpression::VarRef {
                name: func_name, ..
            } = &func.expr
            {
                if func_name == "type-of" {
                    if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                        // Right side must be a string literal type name
                        if let SurfaceExpression::StringLiteral {
                            content: type_name, ..
                        } = &right.expr
                        {
                            let ty = match type_name.as_str() {
                                "Int" => Some(Type::Int),
                                "Float" => Some(Type::Float),
                                "String" => Some(Type::Str),
                                "Bool" => Some(Type::TyCon("Boolean".to_string())),
                                "Seq" => Some(Type::App(
                                    Box::new(Type::TyCon("Seq".to_string())),
                                    Box::new(Type::Unknown),
                                )),
                                "Number" => {
                                    Some(Type::normalize_union(vec![Type::Int, Type::Float]))
                                }
                                _ => None,
                            };
                            return ty.map(|t| Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: t,
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

/// Apply narrowings to a type environment, creating a refined environment for the true branch.
pub(crate) fn apply_narrowings(
    env: &Arc<RwLock<Env>>,
    narrowings: &[Narrowing],
    state: &mut InferState,
) -> Arc<RwLock<Env>> {
    if narrowings.is_empty() {
        return Arc::clone(env);
    }

    let mut new_env_inner = Env::with_parent(Arc::clone(env));

    for narrowing in narrowings {
        match narrowing {
            Narrowing::EqLiteral { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed.
                // Use insert_scheme_named_only: narrowing frames are not resolver scopes,
                // so their entries must not occupy slotted positions.
                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(ty.clone()));
            }
            Narrowing::TypeOf { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed.
                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(ty.clone()));
            }
            Narrowing::HasKey { var, key } => {
                // Get the current type of the variable (if any)
                let current_ty = env
                    .read()
                    .unwrap()
                    .get_scheme(var)
                    .map(|scheme| scheme.body);

                // Create a record type with at least the given key
                let mut fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
                let fresh_type_var = state.fresh_type_var(&crate::rust_span!());
                fields.insert(key.clone(), fresh_type_var);

                // BAS: all tails are Empty. Merge existing record fields if present.
                // Width subtyping handles the openness — the record is known to have the
                // key at runtime, and may have additional fields beyond those annotated.
                let new_ty = if let Some(Type::Dict(current_row)) = current_ty {
                    // Merge existing fields with the new constraint
                    for (k, v) in current_row.fields {
                        fields.insert(k, v);
                    }
                    Type::Dict(Row {
                        fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                } else {
                    // Create a fresh record with just the key constraint
                    Type::Dict(Row {
                        fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                };

                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(new_ty));
            }
        }
    }

    Arc::new(RwLock::new(new_env_inner))
}

/// Collect variable bindings introduced by a pattern, with their types.
///
/// Returns `Vec<(name, type)>` pairs used to extend the TypeEnv before
/// type-checking a match arm body, so that pattern-bound variables are in scope
/// and have the best type available from the scrutinee's static type.
///
/// Type narrowing rules (match-arm-scope sprint):
/// - `Pattern::Variable(name)`: binds `name` to the full scrutinee type.
/// - `Pattern::Dict { fields }`: for each `(key, Pattern::Variable(sub_name))` field,
///   look up `key` in the scrutinee Record type and bind `sub_name` to that field's
///   type. Falls back to `Unknown` when the scrutinee type is not a concrete Record
///   or the key is absent (open rows may carry the field at runtime).
/// - `Pattern::Seq { head, tail }`: head gets `Unknown`, tail gets `Seq(Unknown)`.
/// - `Pattern::Constructor { binding }`: payload gets the field type from the matching NominalVariant
///   when scrutinee is Union or Intersection containing the tag; falls back to `Unknown`.
/// - `Pattern::Or(alts)`: collect from the first alternative only (all alts must bind
///   the same variable set by parser invariant).
/// - `Pattern::Wildcard | Literal | TypeTag | Pin`: no bindings.
pub(crate) fn collect_pattern_bindings(
    pat: &Pattern,
    scrutinee_ty: &Type,
    out: &mut Vec<(String, Type)>,
) {
    match pat {
        Pattern::Pin(_, _) => {
            // Pin patterns are equality checks (or wildcards when out of scope) — they do NOT
            // introduce new variable bindings. New bindings are declared via [case [let name] ...].
            // T-1154: Pin replaced Variable; Variable introduced bindings, Pin never does.
        }
        Pattern::Wildcard | Pattern::Literal(_) => {}
        Pattern::Dict { fields, .. } => {
            for (key, sub_pat) in fields {
                // Narrow the sub-pattern's scrutinee type using the record field type.
                let field_ty = match scrutinee_ty {
                    // Gradual: field not in known set — Unknown for missing field in pattern
                    Type::Dict(row) => row.fields.get(key).cloned().unwrap_or(Type::Unknown),
                    // Union: if all members that are Records agree on the field type, use it.
                    Type::Union(members) => {
                        // Collect field types from all Record members
                        let mut field_types = Vec::new();
                        for member in members {
                            if let Type::Dict(row) = member {
                                if let Some(ty) = row.fields.get(key) {
                                    field_types.push(ty.clone());
                                }
                            }
                        }

                        // If all Record members have this field and all types are equal, use it
                        if !field_types.is_empty() {
                            let first_ty = &field_types[0];
                            if field_types.iter().all(|ty| ty == first_ty) {
                                first_ty.clone()
                            } else {
                                // Gradual: Union members disagree on field type
                                Type::Unknown
                            }
                        } else {
                            // Gradual: no Record member has this field
                            Type::Unknown
                        }
                    }
                    _ => Type::Unknown,
                };
                collect_pattern_bindings(&sub_pat.node, &field_ty, out);
            }
        }
        // Pattern::Seq doesn't exist in current Pattern enum; skip
        Pattern::Constructor { tag, binding } => {
            // Extract the payload type from the scrutinee when it's a Union containing
            // a NominalVariant with matching tag.
            if let Some(b) = binding {
                // Extract the binding variable name for single-field payload resolution.
                // When the binding name matches the sole field name of a single-field variant,
                // the runtime payload is the field value directly (e.g., `[Circle r]` with
                // `Circle r: Int` gives `r: Int`). When names don't match (e.g., `[MyOk p]`
                // with `MyOk n: Int`), the binding receives the whole payload record so that
                // field access `p.n` works correctly.
                let binding_var_name: Option<&str> = match &b.node {
                    Pattern::Pin(name, _) => Some(name.as_str()),
                    _ => None,
                };

                // Helper: resolve the payload type for a single-field or multi-field row.
                //
                // Single-field variant payload resolution:
                // - Positional fields (auto-indexed: "0", "1", ...): always unwrap to the field
                //   value type. The runtime stores the value directly, not as a record.
                //   E.g., [Ok v] where Ok has positional payload → v: a (direct value).
                // - Named fields (e.g., `r: Int`, `n: Int`):
                //   - If binding name matches the field name → unwrap to field value type.
                //     E.g., [Circle r] where Circle has `r: Int` → r: Int.
                //   - If binding name does NOT match → return the full payload record.
                //     E.g., [MyOk p] where MyOk has `n: Int` → p: Record{n:Int} → p.n: Int.
                //
                // Multi-field variants: always return as record (no single-field unwrapping).
                let resolve_payload = |fields: &Row| -> Type {
                    if fields.fields.is_empty() {
                        // Unit variant declared with no fields (e.g., `[type Option [Some] None]`).
                        // The declared type carries no payload information, but B-219 allows unit
                        // variants to be called with a payload: `[Some 42]` produces
                        // `Variant{Some, payload:Some(42)}`. When pattern-matching `[Some v]`
                        // against a scrutinee whose declared type is `NominalVariant{Some, {}}`,
                        // we cannot statically determine the payload type from the declaration alone.
                        //
                        // Returning `Type::Dict({})` (empty record) was wrong: it falsely asserts
                        // that `v` is an empty record, causing spurious type errors when `v` is used
                        // as an Int, Str, etc.
                        //
                        // Returning `Type::Unknown` is the correct gradual escape hatch: the declared
                        // unit variant type genuinely doesn't carry payload type information, so we
                        // fall back to gradual typing for the binding. This is honest and prevents
                        // the false `Record{}` assertion while still allowing the body to type-check.
                        //
                        // Note: `[Some]:` (zero-arg pattern, binding:None) is handled before
                        // `resolve_payload` is called — `resolve_payload` is only reached when
                        // `binding.is_some()`, i.e., when a payload binding `v` is present.
                        Type::Unknown
                    } else if fields.fields.len() == 1 {
                        let field_name = fields.fields.keys().next().unwrap();
                        // Positional fields have auto-indexed names ("0", "1", ...).
                        // Check if the field name is a non-negative integer (positional).
                        let is_positional = field_name.parse::<u64>().is_ok();
                        if is_positional || binding_var_name == Some(field_name.as_str()) {
                            // Positional field or binding name matches: unwrap to field value type
                            fields
                                .fields
                                .get(field_name)
                                .cloned()
                                .unwrap_or(Type::Unknown)
                        } else {
                            // Named field, binding name doesn't match: keep as record for field access
                            Type::Dict(fields.clone())
                        }
                    } else {
                        Type::Dict(fields.clone())
                    }
                };

                let payload_ty = match scrutinee_ty {
                    Type::NominalVariant {
                        tag: variant_tag,
                        fields,
                    } if variant_tag == tag => {
                        // Direct NominalVariant match — extract payload type from fields.
                        resolve_payload(fields)
                    }
                    Type::Union(members) => {
                        // Union: find the NominalVariant member with matching tag
                        let mut matching_fields = None;
                        for member in members {
                            if let Type::NominalVariant {
                                tag: variant_tag,
                                fields,
                            } = member
                            {
                                if variant_tag == tag {
                                    matching_fields = Some(fields.clone());
                                    break;
                                }
                            }
                        }
                        // Gradual: constructor tag not found in Union — payload type unknown
                        matching_fields
                            .map(|f| resolve_payload(&f))
                            .unwrap_or(Type::Unknown)
                    }
                    Type::Intersection(members) => {
                        // Intersection: produced by I-Case3 narrowing when arm_scrutinee_ty is
                        // Intersection([Union([Ok_ty, Err_ty]), NominalVariant("Ok", {})]).
                        // Pass 1: check Union members first — they carry the real field types.
                        // Pass 2: fall back to bare NominalVariants (narrowing markers, may have empty fields).
                        // This ordering ensures we get `r: Int` from `NominalVariant("Circle", {r:Int})`
                        // inside a Union, not `[]` from the bare `NominalVariant("Circle", {})` marker.
                        let mut payload = Type::Unknown;
                        // Pass 1: Union members (real field types)
                        'union_pass: for member in members {
                            if let Type::Union(union_members) = member {
                                for um in union_members {
                                    if let Type::NominalVariant {
                                        tag: variant_tag,
                                        fields,
                                    } = um
                                    {
                                        if variant_tag == tag {
                                            payload = resolve_payload(fields);
                                            break 'union_pass;
                                        }
                                    }
                                }
                            }
                            // Also accept a bare NominalVariant with non-empty fields in pass 1
                            if matches!(payload, Type::Unknown) {
                                if let Type::NominalVariant {
                                    tag: variant_tag,
                                    fields,
                                } = member
                                {
                                    if variant_tag == tag && !fields.fields.is_empty() {
                                        payload = resolve_payload(fields);
                                        break 'union_pass;
                                    }
                                }
                            }
                        }
                        // Pass 2: bare NominalVariant fallback (narrowing markers, possibly empty).
                        // Prefer NominalVariants with non-empty fields (real payload) over
                        // empty-field markers (I-Case3 narrowing artifacts).
                        if matches!(payload, Type::Unknown) {
                            let mut empty_fallback = Type::Unknown;
                            for member in members {
                                if let Type::NominalVariant {
                                    tag: variant_tag,
                                    fields,
                                } = member
                                {
                                    if variant_tag == tag {
                                        if !fields.fields.is_empty() {
                                            // Real payload with fields — use immediately
                                            payload = resolve_payload(fields);
                                            break;
                                        } else if matches!(empty_fallback, Type::Unknown) {
                                            // Empty marker — keep as last resort
                                            empty_fallback = resolve_payload(fields);
                                        }
                                    }
                                }
                            }
                            if matches!(payload, Type::Unknown) {
                                payload = empty_fallback;
                            }
                        }
                        payload
                    }
                    _ => Type::Unknown,
                };
                collect_pattern_bindings(&b.node, &payload_ty, out);
            }
        }
        Pattern::Or(alts) => {
            // Or-patterns: only collect from the first alternative (all alts must bind
            // the same set of variables, so any choice is equivalent for scoping).
            if let Some(first) = alts.first() {
                collect_pattern_bindings(&first.node, scrutinee_ty, out);
            }
        }
        // TypeAssert / TypeAssertPending: bind the inner Pin's name to the narrowed type.
        // `n@Int: body` → TypeAssert { resolved_type: Int, inner: Pin("n") } → bind n: Int.
        // Pin itself produces no binding (see above), so TypeAssert must push explicitly.
        Pattern::TypeAssert {
            resolved_type,
            inner,
        } => {
            if let Some(inner) = inner {
                match &inner.node {
                    // n@Int: bind n to the resolved type from the TypeAssert annotation.
                    Pattern::Pin(name, _) => out.push((name.clone(), resolved_type.clone())),
                    // Non-pin inner: recurse (e.g., nested patterns inside TypeAssert).
                    _ => collect_pattern_bindings(&inner.node, scrutinee_ty, out),
                }
            }
        }
        Pattern::TypeAssertPending { inner, .. } => {
            // TypeAssertPending appears before elaboration; inner Pin binds to scrutinee_ty
            // (resolved_type is not yet available — elaboration hasn't run yet).
            if let Some(inner) = inner {
                match &inner.node {
                    Pattern::Pin(name, _) => out.push((name.clone(), scrutinee_ty.clone())),
                    _ => collect_pattern_bindings(&inner.node, scrutinee_ty, out),
                }
            }
        }
        Pattern::Predicate { .. } => {}
    }
}

/// Extract type parameters from an instance pattern declaration.
///
/// The PatternDecl stores the inner bracket `[a@Int b@Float]` as a single `SurfaceExpression::Dict`
/// binding (auto-indexed entries). This function recursively extracts types from either:
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket; extracts each auto-indexed entry
/// - `SurfaceExpression::Annotated { annotation, .. }` — `a@Type` form; resolves the annotation
/// - `SurfaceExpression::VarRef { .. }` — bare identifier; treated as `Type::Unknown`
/// Resolve a type annotation synchronously for use in instance pattern extraction.
/// Returns a concrete type for primitives and known names, or a fresh TypeVar for
/// complex/unresolvable annotations. Never returns `Type::Unknown` — that would
/// trigger T017 for annotated patterns that simply have unresolvable type names.
fn resolve_annotation_sync(ann: &crate::ast::Spanned<Annotation>, state: &mut InferState) -> Type {
    fn resolve_name(name: &str) -> Type {
        match name {
            "Int" | "Integer" => Type::Int,
            "Float" => Type::Float,
            "String" | "Str" => Type::Str,
            "Bytes" => Type::Bytes,
            "Any" => Type::Any,
            "Unknown" => Type::Unknown,
            other => Type::TyCon(other.to_string()),
        }
    }

    match &ann.node {
        Annotation::Simple(name) => resolve_name(name),
        Annotation::PropertyDict(entries) => {
            // User-written @[type: T  default: ...] form — extract the type from the `type:` key.
            for entry in entries {
                let key_is_type = entry.node.key.as_ref().map_or(false, |k| {
                    matches!(&k.expr, SurfaceExpression::StringLiteral { content: s, .. } if s == "type")
                        || matches!(&k.expr, SurfaceExpression::VarRef { name, .. } if name == "type")
                });
                if key_is_type {
                    if let SurfaceExpression::VarRef { name, .. } = &entry.node.value.expr {
                        return resolve_name(name);
                    }
                    // Complex type expression (e.g., [List a]) — fall back to fresh TypeVar
                    return state.fresh_type_var(&ann.span);
                }
            }
            state.fresh_type_var(&ann.span)
        }
        Annotation::Annotated(name, _) => resolve_name(name),
    }
}

pub(crate) fn extract_pattern_types(
    pattern_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
) -> Result<Vec<Type>, Vec<TypeError>> {
    match &pattern_node.expr {
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            let mut types = Vec::new();
            for binding in bindings {
                extract_binding_types(binding, env, state, &mut types)?;
            }
            Ok(types)
        }
        _ => Err(vec![TypeError::new(
            "instance arm pattern must be a [pattern [...]] or [let ...] declaration",
            pattern_node.span.clone(),
        )]),
    }
}

/// Recursively extract type(s) from a single pattern binding expression.
///
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket `[a@Int b@Float]` (old syntax); expands entries
/// - `SurfaceExpression::LetDecl { bindings }` — inner binding bracket `[let a@Int b@Float]` (new syntax); expands bindings
/// - `SurfaceExpression::Call { func, args, .. }` — implied call `[Type]` or `[Type arg1 arg2]`; infers the call type
/// - `SurfaceExpression::Annotated { annotation, .. }` — `a@Type` form
/// - `SurfaceExpression::VarRef { .. }` — bare identifier → `Type::Unknown`
/// - `SurfaceExpression::Placeholder` — wildcard `_` → `Type::Unknown`
pub(crate) fn extract_binding_types(
    binding: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    types: &mut Vec<Type>,
) -> Result<(), Vec<TypeError>> {
    match &binding.expr {
        // Binding bracket [a@Int b@Float] parsed as auto-indexed Dict (old syntax for multi-param).
        // Named-key dicts like [key: k  value: v] represent a SINGLE structural type (a record),
        // not multiple independent type parameters. Only auto-indexed (keyless) dicts expand.
        SurfaceExpression::Dict(entries) => {
            let all_keyless = entries.iter().all(|e| e.node.key.is_none());
            if all_keyless {
                for entry in entries {
                    extract_binding_types(&entry.node.value, env, state, types)?;
                }
            } else {
                // Named-key dict: single compound type (structural/record type)
                types.push(state.fresh_type_var(&binding.span));
            }
        }
        // Inner binding bracket [let a@Int b@Float] (new unified-bindings syntax)
        SurfaceExpression::LetDecl { bindings } => {
            for sub_binding in bindings {
                extract_binding_types(sub_binding, env, state, types)?;
            }
        }
        // Implied call [Int] or [Result String] — treat as a type name reference.
        // [Int] is parsed as Call { func: VarRef("Int"), args: [], implied: true }.
        // Try to resolve the func as a type annotation; fall back to Unknown on failure.
        SurfaceExpression::Call {
            func,
            args,
            implied: true,
            ..
        } if args.is_empty() => {
            // resolve_annotation is async; push Unknown as fallback in sync context
            types.push(Type::Unknown);
        }
        // Multi-arg implied call [Result String] or other complex type expressions:
        // treat as Unknown (full parametric type resolution is future work).
        SurfaceExpression::Call { .. } => {
            types.push(Type::Unknown);
        }
        // a@Type form: VarRef with annotation
        SurfaceExpression::VarRef {
            annotation: Some(ann),
            ..
        } => {
            // Resolve simple annotations synchronously to avoid T017 false positives.
            // complex annotations fall back to a fresh TypeVar (not Unknown) so T017 is suppressed.
            let ty = resolve_annotation_sync(ann, state);
            types.push(ty);
        }
        // Bare identifier in pattern position: represents a type variable (any type).
        // Use a fresh TypeVar rather than Unknown so that:
        // - T017 ("contains Unknown types") doesn't fire for intentional type variables
        // - T016 coverage violations are still correctly detected (TypeVars in determined
        //   positions that don't appear in determining positions still trigger T016)
        SurfaceExpression::VarRef { .. } => {
            types.push(state.fresh_type_var(&binding.span));
        }
        // Gradual: wildcard placeholder
        SurfaceExpression::Placeholder => {
            types.push(Type::Unknown);
        }
        _ => {
            return Err(vec![TypeError::new(
                "pattern binding must be in form 'a@Type', bare identifier, or [let ...]",
                binding.span.clone(),
            )]);
        }
    }
    Ok(())
}

/// Check if two pattern type lists could overlap (unify).
///
/// This is a pure probe: it saves and restores all mutable fields of `state`
/// that `unify` touches (levels, constraints, kind_env) so that overlap testing
/// never leaks side-effects into the global inference state.
pub(crate) fn patterns_overlap(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, Vec<TypeError>> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_levels = state.levels.clone();
    let saved_constraints = state.constraints.clone();
    let saved_kind_env = state.kind_env.clone();
    let saved_deferred = state.deferred_equalities.clone();
    // Also save subst: improve_functional_dependency writes directly to
    // state.subst (via std::mem::take/replace) rather than through temp_subst.
    let saved_subst = state.subst.clone();

    // Use a temporary substitution so state.subst is also unaffected.
    let _temp_subst = state.subst.clone();
    let overlaps = types_a.iter().zip(types_b.iter()).all(|(ty_a, ty_b)| {
        // Gradual: Unknown is the gradual-typing wildcard for unannotated pattern bindings.
        // Treat Unknown as distinct from any concrete type: a position with Unknown
        // cannot be used to establish overlap (it carries no type information).
        if matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown) {
            return false; // non-overlapping at this position — Unknown is not concrete
        }
        // unify is async; use structural equality as conservative approximation
        ty_a == ty_b || matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown)
    });

    // Restore all mutated fields.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;

    Ok(overlaps)
}

/// Probe whether two type slices can unify (for consistency checks).
/// Returns true if all pairs successfully unify. Side-effect-free — restores state after probe.
pub(crate) fn types_can_unify(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, Vec<TypeError>> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Early bailout: if top-level constructors clearly differ, skip expensive unification.
    for (ty_a, ty_b) in types_a.iter().zip(types_b.iter()) {
        match (ty_a, ty_b) {
            // Clearly disjoint constructors
            (Type::Int, Type::Str)
            | (Type::Int, Type::Float)
            | (Type::Str, Type::Float)
            | (Type::Str, Type::Int)
            | (Type::Float, Type::Int)
            | (Type::Float, Type::Str) => return Ok(false),
            _ => {}
        }
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_levels = state.levels.clone();
    let saved_constraints = state.constraints.clone();
    let saved_kind_env = state.kind_env.clone();
    let saved_deferred = state.deferred_equalities.clone();
    let saved_subst = state.subst.clone();

    // Use a temporary substitution for the probe.
    // Note: this probe uses a separate temp_subst; constraint checking via
    // check_constraints_on_var may miss bindings from the probe. This is acceptable
    // for instance consistency checks where types are typically concrete annotations,
    // but would need to be addressed for general-purpose unification probes.
    let _temp_subst = state.subst.clone();
    let can_unify = types_a.iter().zip(types_b.iter()).all(|(ty_a, ty_b)| {
        ty_a == ty_b || matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown)
    });

    // Restore all mutated fields.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;

    Ok(can_unify)
}

/// Extract parameter indices from a functional dependency variable list.
/// Accepts a single param name (VarRef/Str), a Dict list [a b c], or an implied
/// Call `[a b]` (which the parser produces when `a` is in head position).
/// Returns Vec<usize> of indices into the class params list.
pub(crate) fn extract_param_indices(
    node: &Arc<SurfaceNode>,
    params: &[String],
    span: Span,
) -> Result<Vec<usize>, Vec<TypeError>> {
    let mut indices = Vec::new();

    match &node.expr {
        // Single param: a@Type or just "a"
        SurfaceExpression::VarRef { name, .. }
        | SurfaceExpression::StringLiteral { content: name, .. } => {
            if let Some(idx) = params.iter().position(|p| p == name) {
                indices.push(idx);
            } else {
                return Err(vec![TypeError::new(
                    format!("functional dependency references unknown param '{}'", name),
                    span,
                )]);
            }
        }
        // Multiple params as auto-indexed Dict: produced when bracket contains
        // a literal/annotated head (e.g. `[a@Int b]` → Dict with auto-indexed entries)
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                let param_name = match &entry.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => name,
                    SurfaceExpression::StringLiteral { content: s, .. } => s,
                    _ => {
                        return Err(vec![TypeError::new(
                            "functional dependency param must be an identifier or string",
                            entry.span.clone(),
                        )]);
                    }
                };

                if let Some(idx) = params.iter().position(|p| p == param_name) {
                    indices.push(idx);
                } else {
                    return Err(vec![TypeError::new(
                        format!(
                            "functional dependency references unknown param '{}'",
                            param_name
                        ),
                        entry.span.clone(),
                    )]);
                }
            }
        }
        // Multiple params as implied Call: produced when bracket has identifier in head
        // position, e.g. `[a b]` → Call { func: VarRef("a"), args: [VarRef("b")] }
        SurfaceExpression::Call {
            func,
            args,
            implied: true,
            ..
        } => {
            // Extract the function (head param)
            let head_name = match &func.expr {
                SurfaceExpression::VarRef { name, .. } => name,
                SurfaceExpression::StringLiteral { content: s, .. } => s,
                _ => {
                    return Err(vec![TypeError::new(
                        "functional dependency param must be an identifier or string",
                        func.span.clone(),
                    )])
                }
            };
            if let Some(idx) = params.iter().position(|p| p == head_name) {
                indices.push(idx);
            } else {
                return Err(vec![TypeError::new(
                    format!(
                        "functional dependency references unknown param '{}'",
                        head_name
                    ),
                    func.span.clone(),
                )]);
            }
            // Extract the remaining args
            for arg in args {
                let arg_name = match &arg.expr {
                    SurfaceExpression::VarRef { name, .. } => name,
                    SurfaceExpression::StringLiteral { content: s, .. } => s,
                    _ => {
                        return Err(vec![TypeError::new(
                            "functional dependency param must be an identifier or string",
                            arg.span.clone(),
                        )])
                    }
                };
                if let Some(idx) = params.iter().position(|p| p == arg_name) {
                    indices.push(idx);
                } else {
                    return Err(vec![TypeError::new(
                        format!(
                            "functional dependency references unknown param '{}'",
                            arg_name
                        ),
                        arg.span.clone(),
                    )]);
                }
            }
        }
        _ => {
            return Err(vec![TypeError::new(
                "functional dependency variables must be an identifier or list",
                span,
            )]);
        }
    }

    Ok(indices)
}
