//! Type annotation resolution and type expression parsing.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::{check_surface_expr, contains_unknown_or_top, TypeMap};
use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::error::TypeDiagnostic;
use crate::rust_span;
use crate::type_class::ConstraintArg;
use crate::type_def::TyConDef;
use crate::type_def::Variance;
use crate::types::{Constraint, InferState, Kind, Row, Type};
use crate::value::{HashableValue, Thunk, Value};

/// Convert a variance annotation name to a `Variance` value (T-953).
///
/// Used in `[let a@Covariant b@Contravariant c]` type parameter processing:
/// before checking if the annotation is a typeclass name in ClassEnv, call
/// this function to handle variance annotations first.
///
/// Returns `Some(v)` for known variance names, `None` for everything else
/// (which is then checked against ClassEnv as a typeclass constraint).
#[allow(dead_code)]
pub(crate) fn annotation_to_variance(name: &str) -> Option<Variance> {
    match name {
        "Covariant" => Some(Variance::Covariant),
        "Contravariant" => Some(Variance::Contravariant),
        "Invariant" => Some(Variance::Invariant),
        "Phantom" => Some(Variance::Phantom),
        _ => None,
    }
}

/// Polarity context for variance analysis (T-952, Dolan 2017 §4).
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum Polarity {
    Positive,
    Negative,
}

impl Polarity {
    fn flip(self) -> Self {
        match self {
            Polarity::Positive => Polarity::Negative,
            Polarity::Negative => Polarity::Positive,
        }
    }
}

/// Infer variance for each type parameter by performing polarity analysis on the alias body.
///
/// Implements Dolan 2017 §4 polarity analysis. Walks the body type with a current
/// polarity context, classifying each TypeVar's occurrences:
/// - Appears only in positive positions → Covariant
/// - Appears only in negative positions → Contravariant
/// - Appears in both → Invariant
/// - Never appears → Phantom
///
/// `params` are the FRESH TypeVar names (e.g., `?a₀`, `?b₁`) that params were remapped to.
/// Called after alias body resolution so we operate on real `Type` values.
#[allow(dead_code)]
pub(crate) fn infer_variance(
    body: &Type,
    params: &[String],
    tycon_env: &HashMap<String, Arc<TyConDef>>,
) -> Vec<Variance> {
    let n = params.len();
    let mut pos_seen = vec![false; n];
    let mut neg_seen = vec![false; n];

    walk_polarity(
        body,
        Polarity::Positive,
        params,
        &mut pos_seen,
        &mut neg_seen,
        tycon_env,
    );

    pos_seen
        .iter()
        .zip(neg_seen.iter())
        .map(|(&pos, &neg)| match (pos, neg) {
            (true, false) => Variance::Covariant,
            (false, true) => Variance::Contravariant,
            (true, true) => Variance::Invariant,
            (false, false) => Variance::Phantom,
        })
        .collect()
}

/// Recursive polarity walker for variance inference.
fn walk_polarity(
    ty: &Type,
    pol: Polarity,
    params: &[String],
    pos_seen: &mut Vec<bool>,
    neg_seen: &mut Vec<bool>,
    tycon_env: &HashMap<String, Arc<TyConDef>>,
) {
    match ty {
        Type::TypeVar(name, _) => {
            if let Some(i) = params.iter().position(|p| p == name) {
                match pol {
                    Polarity::Positive => pos_seen[i] = true,
                    Polarity::Negative => neg_seen[i] = true,
                }
            }
        }
        Type::Dict(row) => {
            // Record fields are in covariant (positive) position.
            for t in row.fields.values() {
                walk_polarity(t, pol, params, pos_seen, neg_seen, tycon_env);
            }
            // Uniform tail key and value types also in covariant position.
            // The key type can be a TypeVar (e.g. `[type [let k v] {_@k: v}]`), so both must
            // be visited. Mirrors the B-328 fix in type_unify.rs (lower_levels_check_occurs).
            if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    walk_polarity(k, pol, params, pos_seen, neg_seen, tycon_env);
                }
                walk_polarity(value, pol, params, pos_seen, neg_seen, tycon_env);
            }
        }
        Type::Function {
            params: fn_params,
            ret,
            ..
        } => {
            // Function parameters are contravariant (flip polarity).
            for (_, pt) in fn_params {
                walk_polarity(pt, pol.flip(), params, pos_seen, neg_seen, tycon_env);
            }
            // Return type is covariant.
            walk_polarity(ret, pol, params, pos_seen, neg_seen, tycon_env);
        }
        Type::App(f, arg) => {
            // Check if f is a TyCon with known variance for the argument.
            if let Type::TyCon(tcon_name) = f.as_ref() {
                if let Some(def) = tycon_env.get(tcon_name).cloned() {
                    // Single-argument application: use the first declared variance.
                    if let Some(var) = def.variance.first() {
                        let effective_pol = match var {
                            Variance::Covariant => pol,
                            Variance::Contravariant => pol.flip(),
                            Variance::Invariant => {
                                // Both polarities — invariant in the argument.
                                walk_polarity(
                                    arg,
                                    Polarity::Positive,
                                    params,
                                    pos_seen,
                                    neg_seen,
                                    tycon_env,
                                );
                                walk_polarity(
                                    arg,
                                    Polarity::Negative,
                                    params,
                                    pos_seen,
                                    neg_seen,
                                    tycon_env,
                                );
                                return;
                            }
                            Variance::Phantom => return, // Phantom: argument is not used.
                        };
                        walk_polarity(arg, effective_pol, params, pos_seen, neg_seen, tycon_env);
                        return;
                    }
                }
            }
            // Unknown constructor or multi-arg App(App(..)) chain — conservative: treat as invariant.
            // Walk both f and arg so that TypeVars inside f (e.g., App(App(TyCon("Map"), a), b)
            // where f = App(TyCon("Map"), a)) are visited and do not register as Phantom.
            walk_polarity(f, Polarity::Positive, params, pos_seen, neg_seen, tycon_env);
            walk_polarity(f, Polarity::Negative, params, pos_seen, neg_seen, tycon_env);
            walk_polarity(
                arg,
                Polarity::Positive,
                params,
                pos_seen,
                neg_seen,
                tycon_env,
            );
            walk_polarity(
                arg,
                Polarity::Negative,
                params,
                pos_seen,
                neg_seen,
                tycon_env,
            );
        }
        Type::Union(members) | Type::Intersection(members) => {
            // Union/Intersection members preserve the current polarity (join/meet).
            for m in members {
                walk_polarity(m, pol, params, pos_seen, neg_seen, tycon_env);
            }
        }
        Type::Negation(inner) => {
            // Negation flips polarity.
            walk_polarity(inner, pol.flip(), params, pos_seen, neg_seen, tycon_env);
        }
        // NominalVariant fields are in covariant position — values stored in a variant
        // constructor are accessible (read), so they vary covariantly.
        // This ensures that `a` in `Result[Ok value: a]` is not classified as Phantom.
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => {
            for t in fields.fields.values() {
                walk_polarity(t, pol, params, pos_seen, neg_seen, tycon_env);
            }
            // Also traverse RowTail::Uniform key and value types (T-1032).
            // The key type can be a TypeVar (e.g. `[type [let k v] {_@k: v}]`), so both must
            // be visited. Mirrors the B-328 fix in type_unify.rs (lower_levels_check_occurs).
            if let crate::type_def::RowTail::Uniform {
                key,
                value: value_ty,
            } = &fields.tail
            {
                if let Some(k) = key {
                    walk_polarity(k, pol, params, pos_seen, neg_seen, tycon_env);
                }
                walk_polarity(value_ty, pol, params, pos_seen, neg_seen, tycon_env);
            }
        }
        // S-860: equirecursive-types-core — recurse into the body.
        // The `var` binder is the μ-binder name, not a type parameter; only the body is walked.
        // Variance of type parameters inside the body is unchanged by the μ-binder.
        Type::Recursive { var: _, body } => {
            walk_polarity(body, pol, params, pos_seen, neg_seen, tycon_env);
        }
        // Concrete types (Int, Str, Bool, etc.), TyCon, Unknown, Top, Error — no TypeVar involvement.
        _ => {}
    }
}

pub(crate) async fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    _span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeDiagnostic>> {
    // Create per-annotation-scope mappings for type and row variables.
    // Named row variables (e.g., ...r) in TypeAssert annotations are tracked correctly
    // instead of creating fresh anonymous row vars.
    let mut ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
    let mut ann_mapping_opt = ann_mapping.as_mut();
    let mut row_ann_mapping_opt: Option<&mut HashMap<String, String>> = None;

    let expected = resolve_annotation(
        &annotation.node,
        annotation.span.clone(),
        state,
        constraints,
        &mut ann_mapping_opt,
        &mut row_ann_mapping_opt,
        None,
    )
    .await
    .map_err(|e| vec![e])?;

    // Use checking mode for TypeAssert inner expression (doc/06 §Bidirectional Typing).
    let check_result = check_surface_expr(inner, &expected, env, state, type_map).await;

    // If checking fails, propagate errors (TypeAssert failures are hard type errors).
    if let Err(type_errors) = check_result {
        let has_default = annotation.node.get_property("default").is_some();
        if !has_default {
            return Err(type_errors);
        }
    }

    // Validate the default value type — hard error if the default cannot satisfy the asserted type.
    if let Some(default_node) = annotation.node.get_property("default") {
        let mut local_errors: Vec<TypeDiagnostic> = Vec::new();
        let mut local_stack = Vec::new();
        let default_ty_result = {
            let default_ty = Box::pin(super::typecheck_cek::run_typecheck(
                default_node,
                env,
                state,
                &mut local_errors,
                type_map,
                &mut local_stack,
            ))
            .await;
            if local_errors.is_empty() {
                Ok(default_ty)
            } else {
                Err(local_errors)
            }
        };
        match default_ty_result {
            Ok(default_ty) => {
                // Apply type_vars bindings to both types before comparison — access-chain constraints
                // may have bound TypeVars (e.g., $data.name generates row-variable
                // bindings). Without applying bindings, the comparison uses stale TypeVars.
                // Guard: skip allocation when subst is empty (common case for concrete programs).
                let (default_ty, expected_resolved) = if state.subst_is_empty() {
                    (default_ty, expected.clone())
                } else {
                    (state.apply(&default_ty), state.apply(&expected))
                };
                let passes = Type::is_subtype(&default_ty, &expected_resolved, None)
                    || ((contains_unknown_or_top(&default_ty)
                        || contains_unknown_or_top(&expected_resolved))
                        && Type::is_consistent(&default_ty, &expected_resolved));
                if !passes {
                    return Err(vec![TypeDiagnostic::error(
                        "type-error",
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
        if let SurfaceExpression::StringLiteral {
            content: ref repr_val,
            ..
        } = repr_node.expr
        {
            const VALID_REPRS: &[&str] = &["u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64"];
            if !VALID_REPRS.contains(&repr_val.as_str()) {
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "invalid repr: \"{repr_val}\" — must be one of: {}",
                        VALID_REPRS.join(", ")
                    ),
                    repr_node.span.clone(),
                )]);
            }
            // Check consistency: repr requires a numeric type (Int or Float)
            let is_numeric = matches!(&expected, Type::Int | Type::Float);
            if !is_numeric {
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
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
    let expected = if state.subst_is_empty() {
        expected
    } else {
        state.apply(&expected)
    };

    Ok(expected)
}

/// Resolve an annotated type expression `[@Name $annotation]`.
///
/// If `name == "Fn"`, interprets `$annotation` as a function type specification:
/// - `[@Fn@RetType [Param1 Param2 ...]]` → function type with params and return type
/// - `[@Fn@RetType]` (no param list) → zero-parameter function returning RetType
///
/// Otherwise, resolves `$annotation` as a regular type annotation.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_annotated(
    name: &str,
    annotation: &Spanned<Annotation>,
    _env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    if name == "Fn" {
        resolve_fn_type(
            &annotation.node,
            annotation.span.clone(),
            state,
            constraints,
            ann_mapping,
            row_ann_mapping,
            type_params_scope,
        )
        .await
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
            span,
            state,
            constraints,
            ann_mapping,
            row_ann_mapping,
            type_params_scope,
        )
        .await?;
        Ok(Type::handle(cap_type))
    } else {
        resolve_annotation(
            &annotation.node,
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_fn_metadata(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<(Type, Option<String>), TypeDiagnostic> {
    let mut return_type: Option<Type> = None;
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
                                        let level = state.level;
                                        let fresh = state
                                            .fresh_type_var_with(
                                                Some(name.as_str()),
                                                Some(level),
                                                Kind::Type,
                                                &bind_entry.node.value.span,
                                            )
                                            .0;
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
                                let level = state.level;
                                let fresh = state
                                    .fresh_type_var_with(
                                        Some(name),
                                        Some(level),
                                        Kind::Type,
                                        &name_span,
                                    )
                                    .0;
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
                                let type_var = if let Some(ref mapping) = ann_mapping {
                                    match mapping.get(&typevar_name) {
                                        Some(var) => var.clone(),
                                        None => {
                                            return Err(TypeDiagnostic::error(
                                                "type-error",
                                                format!(
                                                    "kinds: TypeVar '{}' not found in bind: list",
                                                    typevar_name
                                                ),
                                                kind_entry.span.clone(),
                                            ))
                                        }
                                    }
                                } else {
                                    return Err(TypeDiagnostic::error(
                                        "type-error",
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
                                                return Err(TypeDiagnostic::error(
                                                    "type-error",
                                                    format!(
                                                    "unknown kind '{}' (valid: Operator, Label)",
                                                    kind_name
                                                ),
                                                    kind_entry.node.value.span.clone(),
                                                ))
                                            }
                                        };
                                        state.set_kind(type_var, kind);
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
                                        let level = state.level;
                                        let fresh = state
                                            .fresh_type_var_with(
                                                Some(typevar_name.as_str()),
                                                Some(level),
                                                Kind::Type,
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

                                // Parse the class name(s) — can be a single name, [each ...], or [...]
                                // The parser represents `[each Comparable Printable]` as
                                // SurfaceExpression::Call { func: VarRef("each"),
                                // args: [VarRef("Comparable"), VarRef("Printable")] }.
                                // We accept both Dict form (legacy) and Call form (natural parse).
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
                                    SurfaceExpression::Dict(class_list) => {
                                        // Require [each ...] keyword form
                                        let class_entries = if !class_list.is_empty()
                                            && class_list[0].node.key.is_none()
                                        {
                                            if let SurfaceExpression::VarRef { name, .. } =
                                                &class_list[0].node.value.expr
                                            {
                                                if name == "each" {
                                                    // [a: [each Comparable Printable]] — skip 'each'
                                                    &class_list[1..]
                                                } else {
                                                    // [a: [Comparable Printable]] — no 'each', error
                                                    return Err(TypeDiagnostic::error("type-error",
                                                        "constraint class list must start with 'each' keyword: use [each ClassName ...]".to_string(),
                                                        class_list[0].span.clone(),
                                                    ));
                                                }
                                            } else {
                                                return Err(TypeDiagnostic::error("type-error",
                                                    "constraint class list must start with 'each' keyword: use [each ClassName ...]".to_string(),
                                                    class_list[0].span.clone(),
                                                ));
                                            }
                                        } else {
                                            return Err(TypeDiagnostic::error(
                                                "type-error",
                                                "constraint class list cannot be empty".to_string(),
                                                c_entry.node.value.span.clone(),
                                            ));
                                        };

                                        // Multiple classes: iterate and add each
                                        for class_entry in class_entries {
                                            if class_entry.node.key.is_some() {
                                                return Err(TypeDiagnostic::error("type-error",
                                                    "constraint class list must contain only positional entries".to_string(),
                                                    class_entry.span.clone(),
                                                ));
                                            }
                                            match &class_entry.node.value.expr {
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    // Unknown class names deferred to instance resolution.
                                                    state.add_constraint_to(
                                                        constraints,
                                                        name.clone(),
                                                        type_var.clone(),
                                                    );
                                                }
                                                _ => {
                                                    return Err(TypeDiagnostic::error("type-error",
                                                        "constraint class must be a class name (e.g., Comparable)".to_string(),
                                                        class_entry.node.value.span.clone(),
                                                    ));
                                                }
                                            }
                                        }
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

                                        // Validate the class exists in env and get the ClassDecl
                                        let class_decl = {
                                            let env_guard = state.env.read().unwrap();
                                            env_guard.get_class(class_name).ok_or_else(|| {
                                                TypeDiagnostic::error(
                                                    "type-error",
                                                    format!(
                                                        "unknown class '{}' in MPTC constraint",
                                                        class_name
                                                    ),
                                                    c_entry.node.value.span.clone(),
                                                )
                                            })?
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

                                        // Create the MPTC constraint using Arc<ClassDecl>
                                        constraints.push(Constraint::Class {
                                            class: Arc::new(class_decl.clone()),
                                            vars: typevar_names
                                                .into_iter()
                                                .map(ConstraintArg::Var)
                                                .collect(),
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

    // B-361: Warn about unknown fn annotation keys.
    // Valid keys: return, constraint, doc, bind, kinds.
    const VALID_FN_ANNOTATION_KEYS: &[&str] = &["return", "constraint", "doc", "bind", "kinds"];
    for entry in entries {
        if let Some(key_expr) = &entry.node.key {
            if let SurfaceExpression::StringLiteral {
                content: key_name, ..
            } = &key_expr.expr
            {
                if !VALID_FN_ANNOTATION_KEYS.contains(&key_name.as_str()) {
                    state.diagnostics.push(crate::error::TypeDiagnostic {
                        level: crate::error::DiagnosticLevel::Warn,
                        kind: "unknown-type-param",
                        message: format!(
                            "unknown function annotation key '{}' (valid keys: {})",
                            key_name,
                            VALID_FN_ANNOTATION_KEYS.join(", ")
                        ),
                        spans: vec![(key_expr.span.clone(), String::new())],
                        notes: vec![],
                    });
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
#[allow(clippy::too_many_arguments)]
async fn resolve_fn_type(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
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

            if has_fn_key || all_keyed {
                // If any entry has a standard key, or if ALL entries are keyed (custom
                // annotation keys like `[cache: true]`), treat as fn metadata dict.
                // B-355: fn@[only-custom-keys: val] was previously misinterpreted as a
                // return type because has_fn_key was false; now all_keyed triggers this path.
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
                let ty = Type::Function {
                    params: vec![],
                    ret: Box::new(ret),
                    typed_variadics: vec![],
                    rest: None,
                    required_count: 0,
                };
                crate::types::check_kind_wellformed(&ty, &state.kind_env(), span)?;
                Ok(ty)
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
                    type_params_scope,
                    "",
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
            let ty = Type::Function {
                params: vec![],
                ret: Box::new(ret),
                typed_variadics: vec![],
                rest: None,
                required_count: 0,
            };
            crate::types::check_kind_wellformed(&ty, &state.kind_env(), span)?;
            Ok(ty)
        }
    }
}

/// Resolve an annotation in a context where a type expression is expected.
/// Unlike `resolve_annotation`, a PropertyDict is interpreted as a type expression
/// (record type or function type) rather than a property bag.
#[allow(clippy::too_many_arguments)]
async fn resolve_annotation_as_type(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(
                name,
                span,
                state,
                constraints,
                ann_mapping,
                &row_ref,
                type_params_scope,
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
                type_params_scope,
                "",
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_annotation(
    ann: &Annotation,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(
                name,
                span,
                state,
                constraints,
                ann_mapping,
                &row_ref,
                type_params_scope,
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
                    Ok(Type::App(Box::new(outer_ty), Box::new(arg)))
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
                return Ok(Type::Unknown);
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
                                    let level = state.level;
                                    let (v, _) = state.fresh_type_var_with(
                                        Some("_label"),
                                        Some(level),
                                        Kind::Label,
                                        &label_value_node.span,
                                    );
                                    state.kind_env.insert(v.clone(), Kind::Label);
                                    state.type_var_source_names.insert(v.clone(), name.clone());
                                    mapping.insert(name.clone(), v.clone());
                                    v
                                }
                            } else {
                                let level = state.level;
                                let (v, _) = state.fresh_type_var_with(
                                    Some("_label"),
                                    Some(level),
                                    Kind::Label,
                                    &label_value_node.span,
                                );
                                state.kind_env.insert(v.clone(), Kind::Label);
                                v
                            };
                            let current_level = state
                                .get_level(&fresh)
                                .expect("invariant: label var just inserted into type_vars");
                            Ok(Type::TypeVar(fresh, current_level))
                        }
                    }
                    _ => Err(TypeDiagnostic::error(
                        "type-error",
                        "label: value must be a bare name (e.g. `label: l`)",
                        span,
                    )),
                }
            } else {
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

#[allow(clippy::too_many_arguments)]
async fn resolve_property_dict_as_record(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    let dict_result = resolve_type_dict(
        entries,
        span.clone(),
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
        "",
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
                // The property dict is not a type-dict (no `or`/`all`/`without`/`Seq`/`Map`
                // head, no matching record-field shape). Try evaluating it as a type-stage
                // expression — user-defined type-stage combinators like `@[my-combinator args]`
                // are handled by eval_type_stage_expr.
                //
                // Synthesize an Arc<SurfaceNode> from the PropertyDict entries so
                // eval_type_stage_expr can evaluate it in the type-stage environment.
                //
                // Annotation PropertyDicts with all-positional entries whose first entry is
                // a VarRef represent implied calls: @[my-combinator Int String] is parsed as
                // PropertyDict([{key:None, val:VarRef("my-combinator")}, ...]) rather than as
                // a Dict expression. We must detect this case and synthesize a Call node so
                // the evaluator sees a function call, not an integer-keyed dict.
                let _ = e; // suppress the type-dict resolution error
                let synth_node = synthesize_type_stage_node(entries, span.clone());
                // Return Unknown when type-stage evaluation fails: eval error or
                // the result is TypeNode.Recursive/RecursiveRef (deferred).
                Ok(eval_type_stage_expr(&synth_node, state)
                    .await
                    .unwrap_or(Type::Unknown))
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

/// Instantiate a parameterized type alias by substituting type arguments for parameters.
///
/// Given `Pair: [type [a] [first: a second: a]]` and args `[Int]`,
/// builds substitution `{a -> Int}` and applies to body to get `[first: Int second: Int]`.
async fn instantiate_tycon_def(
    alias: &TyConDef,
    type_args: &[Type],
    state: &mut InferState,
) -> Result<Type, TypeDiagnostic> {
    // Build substitution from parameter names to provided types
    let mut type_subst: HashMap<String, Type> = HashMap::new();
    for (param, arg) in alias.params.iter().zip(type_args.iter()) {
        type_subst.insert(param.clone(), arg.clone());
    }

    // Check constraints: each @ClassName annotation on params must have a satisfying instance (T-1101).
    // Iterate constraints and verify that the type argument for each constrained param has an instance.
    for constraint in &alias.constraints {
        if let crate::type_class::Constraint::Class {
            class,
            vars,
            origin_name,
            origin_span,
        } = constraint
        {
            // For single-parameter constraints (the common case), vars[0] is the param name.
            // Look up the type argument for that param in type_subst.
            // Only Var positions have a param name to look up; Ground positions are already resolved.
            if let Some(crate::type_class::ConstraintArg::Var(param_name)) = vars.first() {
                if let Some(arg_type) = type_subst.get(param_name) {
                    // Build a temporary InstanceEnv snapshot from state.env to avoid borrow
                    // conflict: resolve_instance takes &self on InstanceEnv AND &mut state
                    // simultaneously, which Rust's borrow checker would reject if both were
                    // fields of InferState. The snapshot is built once per constraint check.
                    let instance_env = state.get_working_instance_env();
                    let error_span = origin_span.clone().unwrap_or_else(|| rust_span!());
                    match Box::pin(instance_env.resolve_instance(&class.name, arg_type, state))
                        .await
                    {
                        Ok(Some(_)) => {
                            // Constraint satisfied — continue.
                        }
                        Ok(None) => {
                            let constraint_label = origin_name.as_deref().unwrap_or(&class.name);
                            return Err(TypeDiagnostic::error(
                                "type-error",
                                format!(
                                    "type argument `{arg_type}` does not satisfy constraint \
                                     `{constraint_label}` — no instance found for class `{}`",
                                    class.name
                                ),
                                error_span,
                            ));
                        }
                        Err(ambiguity_msg) => {
                            return Err(TypeDiagnostic::error(
                                "type-error",
                                format!(
                                    "ambiguous instances for constraint `{}` with type \
                                     argument `{arg_type}`: {ambiguity_msg}",
                                    class.name
                                ),
                                error_span,
                            ));
                        }
                    }
                }
                // If param_name is not in type_subst, it's a bug (params and constraints were
                // built together in register_type_aliases_env). We silently skip for robustness.
            }
        }
        // HasField constraints (if any) are not relevant to type alias instantiation — skip.
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
/// This is distinct from `InferState::apply` which operates on unification variables.
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
                let level = state.get_level(name).unwrap_or(state.level);
                Type::TypeVar(name.clone(), level)
            }
        }
        Type::Dict(row) => {
            let new_fields: indexmap::IndexMap<String, Type> = row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), apply_type_alias_substitution(v, subst, state)))
                .collect();
            let new_tail = match &row.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(apply_type_alias_substitution(k, subst, state))),
                        value: Box::new(apply_type_alias_substitution(value, subst, state)),
                    }
                }
                other => other.clone(),
            };
            Type::Dict(Row {
                fields: new_fields,
                tail: new_tail,
            })
        }
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
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
            typed_variadics: typed_variadics
                .iter()
                .map(|(name, ty)| {
                    (
                        name.clone(),
                        apply_type_alias_substitution(ty, subst, state),
                    )
                })
                .collect(),
            rest: rest.as_ref().map(|boxed| {
                Box::new((
                    boxed.0.clone(),
                    apply_type_alias_substitution(&boxed.1, subst, state),
                ))
            }),
            ret: Box::new(apply_type_alias_substitution(ret, subst, state)),
            required_count: *required_count,
        },
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
        Type::App(f, arg) => Type::App(
            Box::new(apply_type_alias_substitution(f, subst, state)),
            Box::new(apply_type_alias_substitution(arg, subst, state)),
        ),
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => {
            let new_fields: indexmap::IndexMap<String, Type> = fields
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), apply_type_alias_substitution(v, subst, state)))
                .collect();
            let new_tail = match &fields.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(apply_type_alias_substitution(k, subst, state))),
                        value: Box::new(apply_type_alias_substitution(value, subst, state)),
                    }
                }
                other => other.clone(),
            };
            Type::NominalVariant {
                tycon: tycon.clone(),
                ctor: ctor.clone(),
                fields: Row {
                    fields: new_fields,
                    tail: new_tail,
                },
            }
        }
        // S-860: equirecursive-types-core — recurse into the body.
        // The `var` binder name is NOT a type alias parameter (it is a gensym'd μ-binder)
        // and must NOT be substituted. Substitution applies to the body only.
        Type::Recursive { var, body } => Type::Recursive {
            var: var.clone(),
            body: Box::new(apply_type_alias_substitution(body, subst, state)),
        },
        // All other types (including TyCon) are atomic and don't contain substitutable parameters
        _ => ty.clone(),
    }
}

/// Resolve a type name with recursion guard (used during alias registration).
#[allow(clippy::too_many_arguments)] // Internal helper for type alias expansion
pub(crate) async fn resolve_type_name_with_guard(
    name: &str,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    _current_alias: &str,
    depth: usize,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    // Route lowercase names and all primitive/structural type names through resolve_type_name,
    // which now uses the type-stage env as the canonical source.
    if !name.starts_with(|c: char| c.is_uppercase()) {
        let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
        return resolve_type_name(
            name,
            span,
            state,
            constraints,
            ann_mapping,
            &row_ref,
            type_params_scope,
        )
        .await;
    }

    // Uppercase type name — check for type alias in state.tycon_env
    if let Some(alias) = state.tycon_env.get(name).cloned() {
        // Check if we're in a recursive expansion
        if recursion_guard.contains(name) {
            // Recursive reference detected — return a fresh type variable as the mu-variable
            // for this recursive position. This gives recursive positions a proper type that
            // can be unified with the alias body rather than silently widening to Unknown.
            // Callers see a TypeVar(_tN) that unifies with the alias's expanded type.
            return Ok(state.fresh_type_var(&span));
        }

        // Check arity
        if !alias.params.is_empty() {
            return Err(TypeDiagnostic::error(
                "type-error",
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
    } else if let Some(class_decl) = {
        let env_guard = state.env.read().unwrap();
        env_guard.get_class(name)
    } {
        // T-1197 / T-1206: Class name used in annotation position in a recursive/guarded context.
        // Now that constraints is threaded through, push the Constraint::Class to the caller's
        // collection so the constraint is not dropped.
        let level = state.level;
        let (fresh, fresh_ty) =
            state.fresh_type_var_with(Some(name), Some(level), Kind::Type, &span);
        constraints.push(Constraint::Class {
            class: Arc::new(class_decl),
            vars: vec![ConstraintArg::Var(fresh)],
            origin_name: None,
            origin_span: Some(span),
        });
        Ok(fresh_ty)
    } else {
        Err(TypeDiagnostic::error(
            "type-error",
            format!("undefined type: {}", name),
            span,
        ))
    }
}

/// Unified type-head resolution: given a name and optional already-resolved type arguments,
/// produce a `Type`. This is the single canonical lookup path for all forms:
///   - Bare name: `@Comparable`, `@Integer`, `@Seq` — call with `args = &[]`
///   - Type application: `[Seq a]`, `[Iterable a]`, `[Map K V]` — call with resolved args
///
/// Lookup order (applied identically for bare names and type applications):
///   1. Operator/Label kind annotations (kind constraints, not types)
///   2. class_env (BEFORE tycon_env) — `[Iterable a]` creates constrained TypeVar
///   3. type_stage_scope — type-stage evaluated types from the init program's type-stage docs
///   4. tycon_env → `expand_named` or `instantiate_tycon_def` for structural aliases
///   5. Undefined → TypeDiagnostic
///
/// The lowercase path (ann_mapping, type_params_scope, cross-kind collision) is handled
/// by `resolve_type_name` before calling this function — `resolve_type_head` only handles
/// the case where we have a name that refers to a type constructor or class.
#[allow(clippy::too_many_arguments)]
async fn resolve_type_head(
    name: &str,
    args: &[Type],
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<Type, TypeDiagnostic> {
    // Step 1: Kind constraints — Operator and Label are kinds, not types.
    // When args are present in application position, Operator-kinded names produce App chains.
    match name {
        "Operator" if args.is_empty() => {
            return Err(TypeDiagnostic::error("type-error",
                "Operator is a kind, not a type — annotate a class type parameter as `f@Operator`, not a value expression",
                span,
            ));
        }
        "Label" if args.is_empty() => {
            // Anonymous Label-kinded TypeVar.
            let level = state.level;
            let (fresh, fresh_ty) =
                state.fresh_type_var_with(Some("_label"), Some(level), Kind::Label, &span);
            state.kind_env.insert(fresh.clone(), Kind::Label);
            return Ok(fresh_ty);
        }
        _ => {}
    }

    // Step 1b: Operator-kinded names in application position (e.g., `m` in `[m SomeType]`
    // where `m` was declared `m@Operator` in a class definition).
    // This handles user-defined higher-kinded type parameters.
    if !args.is_empty() {
        if let Some(kind) = state.get_kind(name) {
            if kind.arity() > 0 {
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
                let a_type = args[0].clone();
                if let Type::Operator(op_name) = &a_type {
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
                return Ok(Type::App(
                    Box::new(Type::Operator(name.to_string())),
                    Box::new(a_type),
                ));
            }
        }
    }

    // Step 2: class_env — checked BEFORE tycon_env so that type class names in application
    // position (e.g., `[Iterable a]`) produce a constrained TypeVar rather than TyCon("Iterable").
    // This is the fix for xs@[Iterable a] producing "cannot unify Iterable with Seq".
    if let Some(class_decl) = {
        let env_guard = state.env.read().unwrap();
        env_guard.get_class(name)
    } {
        let level = state.level;
        let (fresh, fresh_ty) =
            state.fresh_type_var_with(Some(name), Some(level), Kind::Type, &span);
        // Build ConstraintArg list: [Var(?c), Ground(arg1), Ground(arg2), ...]
        let mut vars = vec![ConstraintArg::Var(fresh)];
        for arg in args {
            vars.push(ConstraintArg::Ground(arg.clone()));
        }
        constraints.push(Constraint::Class {
            class: Arc::new(class_decl),
            vars,
            origin_name: None,
            origin_span: Some(span),
        });
        return Ok(fresh_ty);
    }

    // Step 3: type_stage_scope — walk the scope chain for pre-computed types.
    // B-588: TypeVar/Class entries require mutable state access (fresh_type_var_with),
    // so we clone the entry and break, then resolve after the immutable borrow ends.
    let mut found_type_stage: Option<crate::type_infer::TypeStageEntry> = None;
    for scope in &state.type_stage_scope {
        if let Some(entry) = scope.get(name) {
            match entry {
                crate::type_infer::TypeStageEntry::Resolved(ty) => {
                    // Fully materialized type (e.g., TypeNode.Int → Type::Int) — return directly.
                    if args.is_empty() {
                        return Ok(ty.clone());
                    }
                    let mut result = ty.clone();
                    for arg in args {
                        result = Type::App(Box::new(result), Box::new(arg.clone()));
                    }
                    return Ok(result);
                }
                crate::type_infer::TypeStageEntry::Function(thunk) => {
                    // Function thunk — parameterized type constructor (e.g., Seq, Result).
                    if args.is_empty() {
                        // Zero-arg reference to a parameterized type constructor (e.g.,
                        // @[is: Seq] or @[narrows: Seq]). The caller wants the unapplied
                        // type constructor — produce TyCon(name) which represents "any
                        // application of this constructor" (Seq of any element type).
                        // This enables annotation-based narrowing for parameterized types
                        // without requiring a type argument (B-546).
                        return Ok(Type::TyCon(name.to_string()));
                    }
                    if let Some(eval_ctx) = &state.eval_ctx {
                        if let Some(ty) = crate::type_normalize::evaluate_resolver_with_thunk(
                            Arc::clone(thunk),
                            args,
                            eval_ctx,
                        )
                        .await
                        {
                            return Ok(ty);
                        }
                    }
                    // eval_ctx unavailable — continue to next scope or Step 4.
                }
                crate::type_infer::TypeStageEntry::TypeVar(kind) => {
                    // B-588: TypeVar entry — clone kind and break to resolve after loop
                    // (avoids borrow conflict: loop borrows state.type_stage_scope immutably,
                    // fresh_type_var_with borrows state mutably).
                    found_type_stage =
                        Some(crate::type_infer::TypeStageEntry::TypeVar(kind.clone()));
                    break;
                }
                crate::type_infer::TypeStageEntry::Class(class_decl) => {
                    // B-588: Class entry — clone decl and break to resolve after loop.
                    found_type_stage =
                        Some(crate::type_infer::TypeStageEntry::Class(class_decl.clone()));
                    break;
                }
            }
        }
    }

    // B-588: Resolve TypeVar/Class entries found in the scope chain (after loop ends,
    // so the immutable borrow of state.type_stage_scope is released).
    match found_type_stage {
        Some(crate::type_infer::TypeStageEntry::TypeVar(kind)) => {
            let level = state.level;
            let (_fresh, fresh_ty) =
                state.fresh_type_var_with(Some(name), Some(level), kind, &span);
            return Ok(fresh_ty);
        }
        Some(crate::type_infer::TypeStageEntry::Class(class_decl)) => {
            let level = state.level;
            let (fresh, fresh_ty) = state.fresh_type_var_with(
                Some(name),
                Some(level),
                crate::type_def::Kind::Type,
                &span,
            );
            let mut vars = vec![crate::type_class::ConstraintArg::Var(fresh)];
            for arg in args {
                vars.push(crate::type_class::ConstraintArg::Ground(arg.clone()));
            }
            constraints.push(crate::type_class::Constraint::Class {
                class: std::sync::Arc::new(class_decl),
                vars,
                origin_name: None,
                origin_span: Some(span),
            });
            return Ok(fresh_ty);
        }
        _ => {} // Resolved/Function handled inside the loop; None = not found
    }

    // Step 4: tycon_env — user-defined nominal types ([type ...] declarations) and
    // opaque Rust types (DirCap, File, etc. registered by imports.rs).
    // These are NOT type-stage types; they are registered during typechecking.
    if let Some(def) = state.tycon_env.get(name).cloned() {
        if !def.constructors.is_empty() {
            let base = Type::TyCon(name.to_string());
            if args.is_empty() {
                return Ok(base);
            }
            let mut result = base;
            for arg in args {
                result = Type::App(Box::new(result), Box::new(arg.clone()));
            }
            return Ok(result);
        }
        if !def.params.is_empty() && !args.is_empty() {
            if args.len() != def.params.len() {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "type alias '{}' expects {} type parameter(s), got {}",
                        name,
                        def.params.len(),
                        args.len()
                    ),
                    span,
                ));
            }
            return Box::pin(instantiate_tycon_def(def.as_ref(), args, state)).await;
        }
        return Ok(expand_named(name, args, state).unwrap_or_else(|| {
            let mut result = Type::TyCon(name.to_string());
            for arg in args {
                result = Type::App(Box::new(result), Box::new(arg.clone()));
            }
            result
        }));
    }

    // Undefined type name.
    Err(TypeDiagnostic::error(
        "type-error",
        format!("undefined type: {}", name),
        span,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_type_name(
    name: &str,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &Option<&HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    match name {
        // Kind constraints — not types, require special handling before the type-stage path.
        "Operator" => Err(TypeDiagnostic::error("type-error",
            "Operator is a kind, not a type — annotate a class type parameter as `f@Operator`, not a value expression",
            span,
        )),
        "Label" => {
            // Anonymous Label-kinded TypeVar. Not a type — creates a fresh label-kinded TypeVar.
            let level = state.level;
            let (fresh, fresh_ty) = state.fresh_type_var_with(Some("_label"), Some(level), Kind::Label, &span);
            state.kind_env.insert(fresh.clone(), Kind::Label);
            Ok(fresh_ty)
        }
        // All fundamental types (Integer, String, Float, Bytes, Never, Any,
        // Proxy, Dict, Expr, Unknown) are declared in builtin_core.llt and resolved through
        // the type_stage_scope/tycon_env chain in resolve_type_head.
        // `@Unknown` resolves via type_stage_scope (seeded with Unknown → Type::Unknown in
        // typecheck_surface_program_annotation_table for test paths, and populated from
        // type-stage documents in production). Operator and Label are handled above.
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
                            return Err(TypeDiagnostic::error("type-error",
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
                        // from state.type_vars. DO NOT reset the level - unification may have
                        // lowered it, and level lowering must be monotone (Kiselyov 2013).
                        let current_level = state
                            .get_level(existing_var)
                            .expect("invariant: annotation var registered in mapping must be in state.type_vars");
                        Ok(Type::TypeVar(existing_var.clone(), current_level))
                    } else {
                        Err(TypeDiagnostic::error("type-error",format!("undefined type: {name}"), span))
                    }
                } else {
                    Err(TypeDiagnostic::error("type-error",format!("undefined type: {name}"), span))
                }
            } else {
                // Uppercase type name — route through the unified resolve_type_head.
                // resolve_type_head checks class_env BEFORE tycon_env (the correct order),
                // then scope-chain lookup, then errors.
                Box::pin(resolve_type_head(name, &[], state, constraints, span)).await
            }
        }
    }
}

/// Expand an alias body type, recursively expanding any nested type alias references.
/// Uses equi-recursive semantics (Amadio & Cardelli 1993) with a depth guard to prevent infinite unfolding.
/// The guard tracks aliases currently being expanded to detect cycles.
#[allow(clippy::too_many_arguments)] // Recursive helper with state threading
#[allow(clippy::only_used_in_recursion)] // state, ann_mapping, row_ann_mapping needed for recursive expansion
fn expand_alias_body_guarded(
    ty: &Type,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    alias_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    span: Span,
) -> Result<Type, TypeDiagnostic> {
    // Depth guard (Amadio & Cardelli 1993)
    const MAX_ALIAS_DEPTH: usize = 256;
    if depth >= MAX_ALIAS_DEPTH {
        return Err(TypeDiagnostic::error(
            "type-error",
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
        Type::Dict(row) => {
            let mut new_fields = indexmap::IndexMap::new();
            for (k, v) in &row.fields {
                new_fields.insert(
                    k.clone(),
                    expand_alias_body_guarded(
                        v,
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
            Ok(Type::Dict(Row {
                fields: new_fields,
                tail: crate::type_def::RowTail::Empty,
            }))
        }
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
        } => {
            let new_params = params
                .iter()
                .map(|(name, p_ty)| {
                    Ok::<_, TypeDiagnostic>((
                        name.clone(),
                        expand_alias_body_guarded(
                            p_ty,
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
            let new_typed_variadics = typed_variadics
                .iter()
                .map(|(name, ty)| {
                    Ok::<_, TypeDiagnostic>((
                        name.clone(),
                        expand_alias_body_guarded(
                            ty,
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
            let new_rest = rest
                .as_ref()
                .map(|boxed| {
                    Ok::<_, TypeDiagnostic>(Box::new((
                        boxed.0.clone(),
                        expand_alias_body_guarded(
                            &boxed.1,
                            state,
                            ann_mapping,
                            row_ann_mapping,
                            alias_guard,
                            current_alias,
                            depth,
                            span.clone(),
                        )?,
                    )))
                })
                .transpose()?;
            let new_ret = Box::new(expand_alias_body_guarded(
                ret,
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
                typed_variadics: new_typed_variadics,
                rest: new_rest,
                ret: new_ret,
                required_count: *required_count,
            })
        }
        Type::Union(members) => {
            let new_members = members
                .iter()
                .map(|m| {
                    expand_alias_body_guarded(
                        m,
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
        Type::App(f, arg) => {
            let new_f = expand_alias_body_guarded(
                f,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span.clone(),
            )?;
            let new_arg = expand_alias_body_guarded(
                arg,
                state,
                ann_mapping,
                row_ann_mapping,
                alias_guard,
                current_alias,
                depth,
                span,
            )?;
            Ok(Type::App(Box::new(new_f), Box::new(new_arg)))
        }
        // For all other types (primitives, type vars, TyCon, etc.), return as-is
        _ => Ok(ty.clone()),
    };

    // Remove current alias from guard after expansion
    alias_guard.remove(current_alias);

    result
}

/// Resolve a type expression with recursion guard for recursive type aliases.
/// This is the internal version used during alias registration.
#[allow(clippy::too_many_arguments)] // Internal helper for recursive type resolution
pub(crate) async fn resolve_type_expr_with_guard(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    const MAX_ALIAS_DEPTH: usize = 256;
    if depth >= MAX_ALIAS_DEPTH {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "recursive type alias '{}' exceeds maximum unfolding depth ({})",
                current_alias, MAX_ALIAS_DEPTH
            ),
            node.span.clone(),
        ));
    }

    match &node.expr {
        SurfaceExpression::StringLiteral { content: s, .. } => Ok(Type::StringLiteral(s.clone())),
        SurfaceExpression::VarRef { name, .. } => {
            resolve_type_name_with_guard(
                name,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                recursion_guard,
                current_alias,
                depth,
                type_params_scope,
            )
            .await
        }
        SurfaceExpression::Dict(entries) => {
            Box::pin(resolve_type_dict_with_guard(
                entries,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                recursion_guard,
                current_alias,
                depth,
                type_params_scope,
            ))
            .await
        }
        _ => {
            // For all other expr types, delegate to normal resolve_type_expr.
            // Most expr types (literals, Annotated, Call) don't recursively reference type aliases,
            // so the guard isn't needed. If we encounter cases where nested aliases cause issues,
            // we can expand this match to handle them explicitly.
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
    }
}

/// Resolve a dict in type position with recursion guard.
#[allow(clippy::too_many_arguments)] // Internal helper for recursive type resolution
async fn resolve_type_dict_with_guard(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    let all_positional = entries.iter().all(|e| e.node.key.is_none());

    // Keyed record dict: `[value: Int  next: Node]` — the most common recursive alias body.
    // When all entries are keyed (or rest `...`), resolve each field value via
    // `resolve_type_expr_with_guard` so the recursion guard is respected for field types.
    // This is the critical path that was previously a dead code path: the old fallthrough
    // to `resolve_type_dict` called `resolve_type_name` (not `_with_guard`), so recursive
    // field references like `next: Node` silently returned the Unknown Pass-1 placeholder
    // instead of a fresh TypeVar.
    //
    // Positional-only forms (Fn types, [Seq T], [Map K V], parameterized aliases,
    // unions) are not keyed, so they fall through to `resolve_type_dict` as before.
    if !all_positional {
        // Reuse the same logic as the tail of resolve_type_dict but route field-value
        // resolution through resolve_type_expr_with_guard.
        let mut fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
        for entry in entries {
            if let SurfaceExpression::Placeholder(..) = &entry.node.value.expr {
                // `...` rest notation: accepted for openness annotation, produces no field.
                continue;
            }
            let key = match &entry.node.key {
                Some(k) => match &k.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => s.clone(),
                    // Field with annotation: `field@Child: Type` (T-1052).
                    // Annotation is now on VarRef directly; use the name field.
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
                    // Mixed keyed+positional dict — fall back to the full resolver.
                    return resolve_type_dict(
                        entries,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                        "",
                    )
                    .await;
                }
            };
            let ty = Box::pin(resolve_type_expr_with_guard(
                &entry.node.value,
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                recursion_guard,
                current_alias,
                depth,
                type_params_scope,
            ))
            .await?;
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
                        let mut member_fields = indexmap::IndexMap::new();
                        member_fields.insert(k, v);
                        Type::Dict(Row {
                            fields: member_fields,
                            tail: crate::type_def::RowTail::Empty,
                        })
                    })
                    .collect();
                return Ok(Type::normalize_intersection(members));
            }
        }

        let ty = Type::Dict(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });
        crate::types::check_kind_wellformed(&ty, &state.kind_env(), span)?;
        return Ok(ty);
    }

    // For remaining positional-only cases (function types, [Seq T], [Map K V],
    // parameterized alias applications, and multi-type unions), delegate to the normal
    // resolver which has the full dispatch logic for those forms.
    resolve_type_dict(
        entries,
        span,
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
        "",
    )
    .await
}

pub(crate) async fn resolve_type_expr(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeDiagnostic> {
    match &node.expr {
        // String literals in type position → Type::StringLiteral (tag-only enum variants).
        // VarRef still goes to resolve_type_name for type alias lookup.
        SurfaceExpression::StringLiteral { content: s, .. } => Ok(Type::StringLiteral(s.clone())),
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
                &row_ref,
                type_params_scope,
            )
            .await
            {
                Ok(ty) => Ok(ty),
                Err(e) if crate::eval::is_constructor_name(name) => {
                    let _ = e;
                    Ok(Type::NominalVariant {
                        tycon: lookup_tycon_for_ctor(state, name),
                        ctor: name.clone(),
                        fields: Row {
                            fields: indexmap::IndexMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        },
                    })
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
                type_params_scope,
                "",
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
                    let required_count = params.len();
                    return Ok(Type::Function {
                        params,
                        ret: Box::new(ret),
                        typed_variadics: vec![],
                        rest: None,
                        required_count,
                    });
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
                        let mut result = Type::TyCon(name.clone());
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
                            result = Type::App(Box::new(result), Box::new(arg));
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
                    // T-1357: The new lookup-table syntax mixes:
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

                        return Ok(Type::NominalVariant {
                            tycon: lookup_tycon_for_ctor(state, name),
                            ctor: name.clone(),
                            fields: Row {
                                fields: fields_map,
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
                    } else {
                        return Ok(Type::NominalVariant {
                            tycon: lookup_tycon_for_ctor(state, name),
                            ctor: name.clone(),
                            fields: Row {
                                fields: indexmap::IndexMap::new(),
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
                    }
                }
            }

            // Lowercase VarRef in implied-call head position with args.
            //
            // Pattern: [a T1 T2 ...] where `a` is lowercase.
            //
            // Two sub-cases:
            // 1. `a` is a hardcoded type combinator keyword (`or`, `all`, `without`): resolve
            //    each arg to a `Type` and combine them via the appropriate type operation.
            // 2. `a` is not a recognized combinator keyword: fall through to
            //    Union([TypeVar(a), T1, T2, ...]).
            //
            // This handles prelude annotations like `[return: [a Null]]` in:
            //   cond: [fn@[return: [a Null] doc: "..."] ...]
            //   when: [fn@[return: [a Null] doc: "..."] ...]
            //   unless: [fn@[return: [a Null] doc: "..."] ...]
            //
            // In these annotations, `a` is a type variable and `Null` is the empty record.
            // The parser sees `[a Null]` as an implied call `Call(VarRef("a"), [VarRef("Null")])`
            // because `a` in head position without `:` or `@` is treated as a function name.
            // The intended meaning is `Union([TypeVar(a), Null])` which we recover via fallback.
            if let SurfaceExpression::VarRef {
                name: func_name, ..
            } = &func.expr
            {
                if func_name.starts_with(|c: char| c.is_lowercase()) && !args.is_empty() {
                    // Hardcoded keyword dispatch for type combinator calls.
                    // Handle [or T1 T2 ...], [all T1 T2 ...], [without T] as Call expressions.
                    // These appear when type combinators are used as VALUE expressions, e.g.
                    // `return: [or a Null]` — the value [or a Null] parses as a Call node,
                    // not as a positional PropertyDict (which the resolve_type_dict hardcoded
                    // path handles). Resolving each arg via resolve_type_expr handles TypeVars
                    // (via ann_mapping), named types, and primitives correctly.
                    let kw = func_name.as_str();
                    if kw == "or" || kw == "all" || kw == "without" {
                        if kw == "without" {
                            if args.len() != 1 {
                                return Err(TypeDiagnostic::error(
                                    "type-error",
                                    format!(
                                        "`without` requires exactly 1 type argument, got {}",
                                        args.len()
                                    ),
                                    node.span.clone(),
                                ));
                            }
                            let inner = Box::pin(resolve_type_expr(
                                &args[0],
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            return Ok(Type::Negation(Box::new(inner)));
                        } else {
                            let mut members = Vec::with_capacity(args.len());
                            for arg in args {
                                let ty = Box::pin(resolve_type_expr(
                                    arg,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                members.push(ty);
                            }
                            return Ok(if kw == "or" {
                                Type::normalize_union(members)
                            } else {
                                Type::normalize_intersection(members)
                            });
                        }
                    }

                    // Type-stage lookup or call failed — this is a genuine resolution error.
                    return Err(TypeDiagnostic::error(
                        "type-error",
                        format!("undefined type-stage function: {func_name}"),
                        func.span.clone(),
                    ));
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_type_dict(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
    tycon_name: &str,
) -> Result<Type, TypeDiagnostic> {
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

    // Hardcoded type-stage keywords: `or`, `all`, `without`.
    //
    // These keywords appear in type expressions as all-positional dicts where the first
    // entry is a bare lowercase VarRef: `[or T1 T2]`, `[all T1 T2]`, `[without T]`.
    // They are recognized as builtin type combinators without requiring the type-stage env.
    //
    //   [or T1 T2 ...]   → Union(T1, T2, ...)
    //   [all T1 T2 ...]  → Intersection(T1, T2, ...)
    //   [without T]      → Negation(T)
    //
    // This block MUST run before TyConDef lookup so that `or`/`all`/`without` are never
    // misidentified as type constructor names.
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name: kw, .. } = &first.node.value.expr {
                    let kw = kw.as_str();
                    if kw == "or" || kw == "all" || kw == "without" {
                        let rest = &entries[1..];
                        if kw == "without" {
                            // `[without T]` — single-argument negation.
                            if rest.len() != 1 {
                                return Err(TypeDiagnostic::error(
                                    "type-error",
                                    format!(
                                        "`without` requires exactly 1 type argument, got {}",
                                        rest.len()
                                    ),
                                    span,
                                ));
                            }
                            let inner = Box::pin(resolve_type_expr(
                                &rest[0].node.value,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            return Ok(Type::Negation(Box::new(inner)));
                        } else {
                            // `[or T1 T2 ...]` or `[all T1 T2 ...]` — variadic union/intersection.
                            let mut members = Vec::with_capacity(rest.len());
                            for entry in rest {
                                let ty = Box::pin(resolve_type_expr(
                                    &entry.node.value,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                ))
                                .await?;
                                members.push(ty);
                            }
                            return Ok(if kw == "or" {
                                Type::normalize_union(members)
                            } else {
                                Type::normalize_intersection(members)
                            });
                        }
                    }
                }
            }
        }
    }

    // Unified type-head application: [Name Arg1 Arg2 ...] or bare [Name].
    //
    // Routes through `resolve_type_head` which checks in the correct order:
    //   1. Operator/Label kind annotations
    //   2. class_env (BEFORE tycon_env) — fixes [Iterable a], [Comparable x], etc.
    //   3. tycon_env → expand_named / instantiate_tycon_def
    //   4. scope-chain lookup → call_strict_resolver
    //   5. Falls through (returns None) for unrecognized names
    //
    // This replaces the three previously separate blocks (TyConDef-based application,
    // kind_env application, and parameterized alias application), which were processed
    // in that order and never checked class_env for names in application position.
    // All three cases now go through the single canonical lookup path.
    //
    // IMPORTANT: We only enter this block when the name is recognizable as a type head
    // (found in class_env, tycon_env, or kind_env as an Operator-kinded name). Uppercase
    // names that are NOT recognized by any of these environments fall through to the
    // nominal variant constructor block below (e.g., `[Ok a]` where Ok is a user variant).
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    // Check whether this name is a known type head. The order matters:
                    // class_env must be checked BEFORE tycon_env (the fix). We do this
                    // lightweight check here to decide whether to enter the path — the
                    // full resolution is done inside resolve_type_head.
                    let in_class_env = {
                        let env_guard = state.env.read().unwrap();
                        env_guard.get_class(name).is_some()
                    };
                    let in_tycon_env = state.tycon_env.contains_key(name.as_str());
                    let in_kind_env = state.get_kind(name.as_str()).is_some_and(|k| k.arity() > 0);
                    let is_kind_keyword = name == "Operator" || name == "Label";

                    if in_class_env || in_tycon_env || in_kind_env || is_kind_keyword {
                        // Resolve argument types from the remaining positional entries.
                        // Keyed entries (constraint:, doc:, bind:, etc.) are metadata — skip them.
                        let mut resolved_args: Vec<Type> = Vec::new();
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
    // This is the new syntax for payload constructors (T-1538). The old form
    // `[File path: String]` (uppercase name in positional head) now produces a parse error.
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
            let mut members: Vec<Type> = Vec::with_capacity(entries.len());
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
                        let variant_fields: indexmap::IndexMap<String, Type> = match &entry
                            .node
                            .value
                            .expr
                        {
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
                        members.push(Type::NominalVariant {
                            tycon: tycon_name.to_string(),
                            ctor: ctor_name,
                            fields: Row {
                                fields: variant_fields,
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
                    }
                    None => {
                        // Positional entry: must be a bare uppercase VarRef (unit constructor).
                        match &entry.node.value.expr {
                            SurfaceExpression::VarRef { name, .. }
                                if crate::eval::is_constructor_name(name) =>
                            {
                                members.push(Type::NominalVariant {
                                    tycon: tycon_name.to_string(),
                                    ctor: name.clone(),
                                    fields: Row {
                                        fields: indexmap::IndexMap::new(),
                                        tail: crate::type_def::RowTail::Empty,
                                    },
                                });
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
                Type::normalize_union(members)
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
                        if let Ok(ty) = resolve_type_name(
                            &tag,
                            span.clone(),
                            state,
                            constraints,
                            ann_mapping,
                            &row_ref,
                            type_params_scope,
                        )
                        .await
                        {
                            return Ok(ty);
                        }
                        // resolve_type_name failed — fall through to NominalVariant check below.
                    }
                    if crate::eval::is_constructor_name(&tag) && !is_builtin_type {
                        // Case 1: Pure positional — [Constructor] or [Constructor PayloadType]
                        let all_remaining_positional =
                            entries[1..].iter().all(|e| e.node.key.is_none());
                        if all_remaining_positional {
                            if entries.len() == 1 {
                                return Ok(Type::NominalVariant {
                                    tycon: tycon_name.to_string(),
                                    ctor: tag.to_string(),
                                    fields: Row {
                                        fields: indexmap::IndexMap::new(),
                                        tail: crate::type_def::RowTail::Empty,
                                    },
                                });
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
                                    return Ok(Type::NominalVariant {
                                        tycon: tycon_name.to_string(),
                                        ctor: tag.to_string(),
                                        fields: Row {
                                            fields,
                                            tail: crate::type_def::RowTail::Empty,
                                        },
                                    });
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
                            return Ok(Type::NominalVariant {
                                tycon: tycon_name.to_string(),
                                ctor: tag.to_string(),
                                fields: Row {
                                    fields: variant_fields,
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            });
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
            let members: Vec<Type> = entries
                .iter()
                .map(|e| {
                    let name = match &e.node.value.expr {
                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                        _ => unreachable!(),
                    };
                    Type::NominalVariant {
                        tycon: tycon_name.to_string(),
                        ctor: name,
                        fields: Row {
                            fields: indexmap::IndexMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        },
                    }
                })
                .collect();
            return Ok(Type::normalize_union(members));
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

    let mut fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    let mut has_rest = false; // tracks if `...` is present (BAS: openness via width subtyping)
                              // Column constraint: `{_ : V}` or `{_@K : V}` annotation syntax (T-950).
                              // At most one `_` per row type; duplicate produces a type error.
    let mut uniform_tail: Option<crate::type_def::RowTail> = None;

    for entry in entries {
        if let SurfaceExpression::Placeholder(_name, _) = &entry.node.value.expr {
            // BAS: `...` annotations express user intent for openness; under BAS width
            // subtyping all records are closed — is_subtype handles extra fields.
            has_rest = true;
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
                    Some(Box::new(key_t))
                }
                _ => None, // plain `_`: no key type constraint
            };
            uniform_tail = Some(crate::type_def::RowTail::Uniform {
                key: key_ty,
                value: Box::new(value_ty),
            });
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

    // When a `{_ : V}` column constraint is present (T-950), skip the intersection-splitting
    // path and produce a single Record with the Uniform tail. Named fields and uniform tail
    // coexist: `{x: Int, _ : Str}` → `Record { fields: {x: Int}, tail: Uniform(None, Str) }`.
    let effective_tail = uniform_tail.unwrap_or(crate::type_def::RowTail::Empty);

    // Multi-field record annotation → intersection of closed single-field records.
    //
    // `@[x: Int  y: String]` → `Intersection([{x: Int}, {y: String}])`
    //
    // Only applies when there is no uniform tail — a `{_ : V}` annotation anchors all
    // named fields to a single Record (not split), so the intersection path is skipped.
    //
    // SHARED TYPE VARIABLE GUARD: If any TypeVar name appears in more than one field,
    // fall back to the closed Record. Splitting into single-field members would cause
    // each member to independently bind the shared TypeVar to a different concrete value
    // during unification, producing spurious "cannot unify X with Y" errors.
    if fields.len() >= 2 && !has_rest && matches!(effective_tail, crate::type_def::RowTail::Empty) {
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
                    let mut member_fields = indexmap::IndexMap::new();
                    member_fields.insert(k, v);
                    Type::Dict(Row {
                        fields: member_fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                })
                .collect();
            return Ok(Type::normalize_intersection(members));
        }
    }

    let ty = Type::Dict(Row {
        fields,
        tail: effective_tail,
    });
    crate::types::check_kind_wellformed(&ty, &state.kind_env(), span)?;
    Ok(ty)
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
/// Returns `None` if:
/// - The value is not a Variant, or has no payload.
/// - The payload cannot be materialized (eval error).
/// - The materialized payload is not a Dict.
///
/// Used by `typenode_value_to_type` to access named fields of structural TypeNode variants.
async fn variant_payload_dict(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<HashMap<String, Value>> {
    let payload_id = match val {
        Value::Variant {
            payload: Some(id), ..
        } => id.clone(),
        _ => return None,
    };
    let payload_thunk = ctx.get_thunk(payload_id);
    let payload_val = crate::eval::materialize(&payload_thunk, None, ctx)
        .await
        .ok()?;
    match payload_val {
        Value::Dict(dict) => {
            // Extract each string-keyed field, materializing the field value.
            let mut fields = HashMap::new();
            for (key, thunk) in &dict {
                if let HashableValue::Str(k) = key {
                    if let Ok(v) = crate::eval::materialize(thunk, None, ctx).await {
                        fields.insert(k.to_string(), v);
                    }
                }
            }
            Some(fields)
        }
        _ => None,
    }
}

/// Collect a TypeNode children Dict (`[Map Int TypeNode]`) into a Vec of `Type`.
///
/// TypeNode fields like `Union.types`, `Intersect.types`, `Arrow.params`, and
/// `TypeApplication.args` are now integer-keyed Dicts of TypeNode values.
/// Each value is converted via `typenode_value_to_type`.
///
/// Returns `None` if any element fails to convert or the input is not a Dict.
async fn collect_typenode_seq(
    dict_val: Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Vec<Type>> {
    // T-1555: Value::Annotated is removed; no unwrapping needed.
    let dict = match dict_val {
        Value::Dict(d) => d,
        _ => return None,
    };

    let mut result = Vec::new();
    let mut i = 0i64;
    loop {
        match dict.get(&HashableValue::Int(i)) {
            Some(thunk) => {
                let val = crate::eval::materialize(thunk, None, ctx).await.ok()?;
                let ty = Box::pin(typenode_value_to_type(&val, ctx)).await?;
                result.push(ty);
                i += 1;
            }
            None => return Some(result),
        }
    }
}

/// Convert a TypeNode `Value` to a `Type`.
///
/// Handles TypeNode Variant values produced by the type-stage evaluator:
///
/// **TypeNode Variant values** — `Variant { tag: "TypeNode.Int" }`, `Variant { tag:
/// "TypeNode.Union", payload: ... }` etc., produced by the TypeNode ADT declared in the
/// type-stage prelude (T-1058/T-1061). Matched by tag prefix `"TypeNode."`.
///
/// Returns `None` if the value cannot be recognized as a Type.
///
/// **Coverage:** All structural TypeNode variants are handled: Union, Intersect, Negation,
/// Record, Arrow, TypeConstructor, TypeApplication, TypeVar, Recursive, RecursiveRef.
///
/// Public (crate-internal) re-export; the implementation is `typenode_value_to_type`.
pub(crate) async fn typenode_value_to_type_pub(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Type> {
    typenode_value_to_type(val, ctx).await
}

fn typenode_value_to_type<'a>(
    val: &'a Value,
    ctx: &'a Arc<crate::eval::EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Type>> + 'a>> {
    Box::pin(async move {
        match val {
            // T-1555: Value::Annotated is removed; annotated bindings no longer need unwrapping.

            // TypeNode Variant values produced by the TypeNode ADT (T-1058 / T-1061).
            Value::Variant {
                tycon,
                ctor,
                payload: _,
            } => {
                let tag = format!("{}.{}", tycon, ctor);
                match tag.as_str() {
                    // ── Primitive leaf constructors ──────────────────────────────────────
                    // No payload — map directly to concrete Type variants.
                    "TypeNode.Int" => Some(Type::Int),
                    "TypeNode.Float" => Some(Type::Float),
                    "TypeNode.String" => Some(Type::Str),
                    // TypeNode.Bool: Boolean is a prelude-defined TyCon. Return None here so
                    // callers fall through to as_type_dispatch. Note: as_type_dispatch currently
                    // cannot resolve TypeNode.Bool to a concrete Type via the prelude as-type chain,
                    // so compound TypeNode expressions containing Bool sub-nodes will fail resolution.
                    // A future fix would look up Boolean via the TyCon env directly.
                    "TypeNode.Bool" => None,
                    "TypeNode.Unknown" => Some(Type::Unknown),
                    // TypeNode.Top is the sound lattice top (τ <: Top for all τ).
                    // Rust represents this as Type::Any (which IS the top type in the lattice).
                    // Distinct from TypeNode.Unknown (the gradual ? type, not in the subtype lattice).
                    "TypeNode.Top" => Some(Type::Any),
                    "TypeNode.Never" => Some(Type::Never),
                    "TypeNode.Absent" => Some(Type::Dict(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    })),
                    "TypeNode.Bytes" => Some(Type::Bytes),
                    "TypeNode.Proxy" => Some(Type::Proxy),
                    // Any callable — variadic function with zero required params.
                    "TypeNode.AnyFn" => Some(Type::Function {
                        params: vec![],
                        ret: Box::new(Type::Any),
                        typed_variadics: vec![],
                        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
                        required_count: 0,
                    }),

                    // ── Union ─────────────────────────────────────────────────────────────
                    // TypeNode.Union { types: [Seq TypeNode] } → Type::normalize_union(members)
                    "TypeNode.Union" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let types_val = fields.get("types")?.clone();
                        let members = collect_typenode_seq(types_val, ctx).await?;
                        if members.is_empty() {
                            return None; // Empty union is ill-formed — fall back to Unknown.
                        }
                        Some(Type::normalize_union(members))
                    }

                    // ── Intersect ────────────────────────────────────────────────────────
                    // TypeNode.Intersect { types: [Seq TypeNode] } → Type::normalize_intersection(members)
                    "TypeNode.Intersect" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let types_val = fields.get("types")?.clone();
                        let members = collect_typenode_seq(types_val, ctx).await?;
                        if members.is_empty() {
                            return None; // Empty intersection is ill-formed — fall back to Unknown.
                        }
                        Some(Type::normalize_intersection(members))
                    }

                    // ── Negation ─────────────────────────────────────────────────────────
                    // TypeNode.Negation { inner: TypeNode } → Type::Negation(Box<Type>)
                    "TypeNode.Negation" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let inner_val = fields.get("inner")?.clone();
                        let inner_type = typenode_value_to_type(&inner_val, ctx).await?;
                        Some(Type::Negation(Box::new(inner_type)))
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
                    "TypeNode.Dict" => {
                        let payload_fields = variant_payload_dict(val, ctx).await?;
                        let fields_val = payload_fields.get("fields")?.clone();
                        let open_val = payload_fields.get("open")?.clone();

                        // `fields` is a Dict (Map String TypeNode) — string-keyed, values are TypeNodes.
                        let record_fields = match fields_val {
                            Value::Dict(ref dict) => {
                                let mut out: indexmap::IndexMap<String, Type> =
                                    indexmap::IndexMap::new();
                                for (key, thunk) in dict {
                                    if let HashableValue::Str(k) = key {
                                        let v = crate::eval::materialize(thunk, None, ctx)
                                            .await
                                            .ok()?;
                                        let ty = typenode_value_to_type(&v, ctx).await?;
                                        out.insert(k.to_string(), ty);
                                    }
                                }
                                out
                            }
                            _ => indexmap::IndexMap::new(), // Empty or unrecognized fields → empty record
                        };

                        // Optional key-type/key: and value-type/value: fields enable Map[K:V] encoding.
                        // Both `key-type:` and `key:` are accepted (likewise `value-type:` / `value:`).
                        // If key-type (or key:) is present, build a typed-key Uniform tail regardless of `open`.
                        // row-polymorphic: 1 → sentinel RowVar; caller (resolve_type_head) will
                        // replace it with a fresh RowVar using InferState. This is a general protocol:
                        // any TypeNode that needs a fresh row var sets row-polymorphic: 1.
                        let tail = if let Some(key_type_val) = payload_fields
                            .get("key-type")
                            .or_else(|| payload_fields.get("key"))
                            .cloned()
                        {
                            let key_ty = typenode_value_to_type(&key_type_val, ctx).await?;
                            // value-type / value: defaults to Any when absent.
                            let value_ty = if let Some(vt_val) = payload_fields
                                .get("value-type")
                                .or_else(|| payload_fields.get("value"))
                                .cloned()
                            {
                                typenode_value_to_type(&vt_val, ctx).await?
                            } else {
                                Type::Any
                            };
                            crate::type_def::RowTail::Uniform {
                                key: Some(Box::new(key_ty)),
                                value: Box::new(value_ty),
                            }
                        } else if matches!(&open_val, Value::Bool(true))
                            || matches!(&open_val, Value::Int(n) if *n != 0)
                        {
                            // Open record: any field value is allowed (Top = Any).
                            // Dict <: open-record: Null <: Record <: Dict hierarchy.
                            crate::type_def::RowTail::Uniform {
                                key: None,
                                value: Box::new(Type::Any),
                            }
                        } else {
                            crate::type_def::RowTail::Empty
                        };

                        Some(Type::Dict(Row {
                            fields: record_fields,
                            tail,
                        }))
                    }

                    // ── Arrow ─────────────────────────────────────────────────────────────
                    // TypeNode.Arrow { params: [Seq TypeNode], result: TypeNode }
                    // → Type::Function { params: Vec<(None, Type)>, ret: Box<Type>, variadic: false }
                    "TypeNode.Arrow" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let params_val = fields.get("params")?.clone();
                        let result_val = fields.get("result")?.clone();

                        let param_types = collect_typenode_seq(params_val, ctx).await?;
                        let ret_type = typenode_value_to_type(&result_val, ctx).await?;

                        let params: Vec<(Option<String>, Type)> =
                            param_types.into_iter().map(|t| (None, t)).collect();

                        let required_count = params.len();
                        Some(Type::Function {
                            params,
                            ret: Box::new(ret_type),
                            typed_variadics: vec![],
                            rest: None,
                            required_count,
                        })
                    }

                    // ── TypeConstructor ───────────────────────────────────────────────────
                    // TypeNode.TypeConstructor { name: String }
                    // Bare (transient): name without '.' → Type::TyCon(name) for expansion
                    // Qualified (leaf): name with '.' → Type::NominalVariant or TyCon leaf
                    "TypeNode.TypeConstructor" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let name_val = fields.get("name")?;
                        let name = name_val.as_str()?.to_string();
                        // Map known primitive names to their concrete Type variants.
                        // (These arise when param-token TypeConstructors from parametric bodies
                        // are passed to typenode_value_to_type without being substituted first.)
                        match name.as_str() {
                            "Int" | "Integer" => Some(Type::Int),
                            "Float" => Some(Type::Float),
                            "String" | "Str" => Some(Type::Str),
                            "Unknown" => Some(Type::Unknown),
                            "Never" => Some(Type::Never),
                            "Absent" => Some(Type::Dict(Row {
                                fields: indexmap::IndexMap::new(),
                                tail: crate::type_def::RowTail::Empty,
                            })),
                            _ => Some(Type::TyCon(name)),
                        }
                    }

                    // ── TypeApplication ───────────────────────────────────────────────────
                    // TypeNode.TypeApplication { ctor: TypeNode, args: [Seq TypeNode] }
                    // → left-associative Type::App chain: App(App(ctor, args[0]), args[1])...
                    "TypeNode.TypeApplication" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let ctor_val = fields.get("ctor")?.clone();
                        let args_val = fields.get("args")?.clone();

                        let ctor_type = typenode_value_to_type(&ctor_val, ctx).await?;
                        let arg_types = collect_typenode_seq(args_val, ctx).await?;

                        if arg_types.is_empty() {
                            // Zero-arg application — return the constructor itself.
                            return Some(ctor_type);
                        }

                        // Build left-associative App chain.
                        let mut result = ctor_type;
                        for arg in arg_types {
                            result = Type::App(Box::new(result), Box::new(arg));
                        }
                        Some(result)
                    }

                    // ── TypeVar ───────────────────────────────────────────────────────────
                    // TypeNode.TypeVar { name: String, level: Int }
                    // → Type::TypeVar(name, level)
                    "TypeNode.TypeVar" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let name_val = fields.get("name")?;
                        let level_val = fields.get("level")?;
                        let name = name_val.as_str()?.to_string();
                        let level = match level_val {
                            Value::Int(n) => *n as u32,
                            _ => 0u32,
                        };
                        Some(Type::TypeVar(name, level))
                    }

                    // ── Recursive ────────────────────────────────────────────────────────
                    // TypeNode.Recursive { var: String, body: TypeNode }
                    // → Type::Recursive { var, body: Box<Type> }
                    //
                    // The `var` field is a String binder name (e.g., "𝜇List"). The `body`
                    // field is a TypeNode that may contain TypeNode.RecursiveRef nodes with
                    // the same `name`, which map to Type::TypeVar(name, 0) sentinels.
                    "TypeNode.Recursive" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let var_val = fields.get("var")?;
                        let var = var_val.as_str()?.to_string();
                        let body_val = fields.get("body")?.clone();
                        let body = Box::pin(typenode_value_to_type(&body_val, ctx)).await?;
                        Some(Type::Recursive {
                            var,
                            body: Box::new(body),
                        })
                    }

                    // ── RecursiveRef ──────────────────────────────────────────────────────
                    // TypeNode.RecursiveRef { name: String }
                    // → Type::TypeVar(name, 0)
                    //
                    // RecursiveRef is a leaf node marking a back-reference to the enclosing
                    // TypeNode.Recursive binder with the same `name`. In the Rust Type enum
                    // this is represented as TypeVar(name, 0) — the same sentinel produced by
                    // expand_named Step 4 (cycle detection). Level 0 is used because recursive
                    // self-references are not inference variables and must never be generalized.
                    "TypeNode.RecursiveRef" => {
                        let fields = variant_payload_dict(val, ctx).await?;
                        let name_val = fields.get("name")?;
                        let name = name_val.as_str()?.to_string();
                        Some(Type::TypeVar(name, 0))
                    }

                    // Unknown tag — not a recognized TypeNode constructor.
                    _ => None,
                }
            }

            // Constructor dict — a tinct ADT declaration like `Color: [type Red Green Blue]`
            // evaluates to `{ Red: Variant("Color.Red"), Green: Variant("Color.Green"), ... }`.
            // Detect the pattern: all values are Variants sharing the same qualified prefix.
            // If so, return Type::TyCon(prefix) — the name of the declared type.
            Value::Dict(entries) if !entries.is_empty() => {
                let mut prefix: Option<String> = None;
                let mut all_match = true;
                for (_key, thunk) in entries {
                    if let Some(val) = thunk.try_get_value().cloned() {
                        match val {
                            Value::Variant { tycon, .. } => match &prefix {
                                None => prefix = Some(tycon.to_string()),
                                Some(existing) if existing.as_str() == tycon.as_ref() => {}
                                _ => {
                                    all_match = false;
                                    break;
                                }
                            },
                            // Function entries (payload constructors) — still count as the same ADT
                            Value::Function { .. } | Value::Builtin(_) => {}
                            _ => {
                                all_match = false;
                                break;
                            }
                        }
                    } else {
                        // Thunk not yet materialized — can't determine
                        all_match = false;
                        break;
                    }
                }
                if all_match {
                    prefix.map(Type::TyCon)
                } else {
                    None
                }
            }

            // Not a recognizable TypeNode or ADT value.
            _ => None,
        }
    })
}

/// Convert a resolved `Type` back to a `Value::Variant` representing the corresponding
/// TypeNode ADT constructor.
///
/// This is the inverse of `typenode_value_to_type` for the subset of `Type` variants that
/// have a direct TypeNode representation. Used by the implied-call path in `resolve_type_expr`
/// when calling type-stage functions (like `or`, `all`, `without`) with arguments that may
/// contain TypeVars — the args are first resolved to `Type` via `resolve_type_expr`, then
/// converted to `TypeNode Value`s here before being passed to `eval_type_stage_value`.
///
/// Returns `None` for `Type` variants that cannot be faithfully round-tripped through the
/// TypeNode ADT (e.g., complex structural types not yet fully supported). Callers should fall
/// back to the Union interpretation when `None` is returned.
fn type_to_typenode_value<'a>(
    ty: &'a Type,
    ctx: &'a Arc<crate::eval::EvalContext>,
    span: Span,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Value>> + 'a>> {
    Box::pin(async move {
        // mk_arc: build an Arc<Thunk> for dict value insertion (T-1772: Value::Dict stores Arc<Thunk>)
        let mk_arc = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, span.clone())) };
        // alloc_payload: wrap a Value::Dict as Arc<Thunk> for Variant payload
        let alloc_payload = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, span.clone())) };

        Some(match ty {
            Type::Int => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Int"),
                payload: None,
            },
            Type::Float => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Float"),
                payload: None,
            },
            Type::Str => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("String"),
                payload: None,
            },
            Type::Bytes => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Bytes"),
                payload: None,
            },
            Type::Any => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Top"),
                payload: None,
            },
            Type::Never => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Never"),
                payload: None,
            },
            Type::Unknown => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Unknown"),
                payload: None,
            },
            Type::Proxy => Value::Variant {
                tycon: Arc::from("TypeNode"),
                ctor: Arc::from("Proxy"),
                payload: None,
            },
            Type::TypeVar(name, level) => {
                let mut payload: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
                    indexmap::IndexMap::new();
                payload.insert(
                    HashableValue::Str("name".into()),
                    mk_arc(crate::value::string_val(name)),
                );
                payload.insert(
                    HashableValue::Str("level".into()),
                    mk_arc(Value::Int(*level as i64)),
                );
                let payload_tid = alloc_payload(Value::Dict(payload));
                Value::Variant {
                    tycon: Arc::from("TypeNode"),
                    ctor: Arc::from("TypeVar"),
                    payload: Some(payload_tid),
                }
            }
            Type::Dict(row) => {
                // TypeNode.Dict { fields: dict_of_typenodes, open: Bool }
                let mut fields_map: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
                    indexmap::IndexMap::new();
                for (k, v_ty) in &row.fields {
                    let field_tn =
                        Box::pin(type_to_typenode_value(v_ty, ctx, span.clone())).await?;
                    fields_map.insert(HashableValue::Str(k.clone().into()), mk_arc(field_tn));
                }
                let open = matches!(row.tail, crate::type_def::RowTail::Uniform { .. });
                let mut payload: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
                    indexmap::IndexMap::new();
                payload.insert(
                    HashableValue::Str("fields".into()),
                    mk_arc(Value::Dict(fields_map)),
                );
                payload.insert(
                    HashableValue::Str("open".into()),
                    mk_arc(Value::Int(if open { 1 } else { 0 })),
                );
                let payload_tid = alloc_payload(Value::Dict(payload));
                Value::Variant {
                    tycon: Arc::from("TypeNode"),
                    ctor: Arc::from("Dict"),
                    payload: Some(payload_tid),
                }
            }
            Type::Union(members) => {
                let mut member_vals: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
                    indexmap::IndexMap::new();
                for (i, m) in members.iter().enumerate() {
                    let m_tn = Box::pin(type_to_typenode_value(m, ctx, span.clone())).await?;
                    member_vals.insert(HashableValue::Int(i as i64), mk_arc(m_tn));
                }
                let mut payload: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
                    indexmap::IndexMap::new();
                payload.insert(
                    HashableValue::Str("types".into()),
                    mk_arc(Value::Dict(member_vals)),
                );
                let payload_tid = alloc_payload(Value::Dict(payload));
                Value::Variant {
                    tycon: Arc::from("TypeNode"),
                    ctor: Arc::from("Union"),
                    payload: Some(payload_tid),
                }
            }
            // For types we cannot faithfully represent as a TypeNode value, return None.
            // The caller falls back to the Union interpretation.
            _ => return None,
        })
    })
}

/// Evaluate a type-stage tinct function value with the given arguments and convert the
/// result to a `Type`.
///
/// This is the inner call protocol for type-stage dispatch — used to invoke any type-stage
/// function value with pre-materialized argument values.
///
/// ## Behaviour
///
/// 1. Allocates materialized thunks for each argument value in `args`.
/// 2. Calls `fn_val` via `invoke_function` using the EvalContext from `state.eval_ctx`.
/// 3. Materializes the result thunk.
/// 4. Converts the result via `typenode_value_to_type`.
///
/// Returns `Err(TypeDiagnostic)` if:
/// - `fn_val` is not a function value.
/// - `state.eval_ctx` is `None` (EvalContext unavailable).
/// - Function invocation or materialization fails.
/// - The result cannot be converted to a `Type` (unrecognized TypeNode tag).
///
/// ## Usage
///
/// Callable when a type-stage function value is already in hand (e.g., an `as-type:` fn
/// extracted from a constructor annotation).
pub(crate) async fn eval_type_stage_value(
    fn_val: &Value,
    args: &[Value],
    state: &mut InferState,
) -> Result<Type, TypeDiagnostic> {
    let origin_span = rust_span!();

    // Use the EvalContext from tinct's evaluation pipeline — capabilities flow in from tinct.
    let ctx = match state.eval_ctx.clone() {
        Some(ctx) => ctx,
        None => {
            return Err(TypeDiagnostic::error(
                "type-error",
                "type-stage evaluation requires an EvalContext from the tinct pipeline",
                origin_span.clone(),
            ))
        }
    };

    // Build materialized Arc<Thunk> for each argument.
    let arg_thunks: Vec<Arc<Thunk>> = args
        .iter()
        .map(|v| Arc::new(Thunk::value(v.clone(), origin_span.clone())))
        .collect();

    // Dispatch: fn_val must be a user-defined function (not a builtin).
    // Builtin type-stage functions are not expected in as-type dispatch paths.
    // T-1555: Value::Annotated is removed; function values are no longer wrapped.
    let result_thunk = match fn_val {
        Value::Function {
            ref params,
            ref body,
            ref closure_env,
            ..
        } => {
            let call_ctx = crate::eval_call::CallContext {
                params,
                body,
                closure_env: Arc::clone(closure_env),
                positional: &arg_thunks,
                named: None,
                default_env_id: 0,
                call_span: origin_span.clone(),
                ctx: &ctx,
            };
            crate::eval_call::invoke_function(&call_ctx)
                .await
                .map_err(|e| {
                    TypeDiagnostic::error(
                        "type-error",
                        format!("type-stage function call failed: {e}"),
                        origin_span.clone(),
                    )
                })?
        }
        // Not a function — as-type dispatch requires a callable value.
        _ => {
            return Err(TypeDiagnostic::error(
                "type-error",
                "eval_type_stage_value: argument is not a function value",
                origin_span,
            ))
        }
    };

    // Materialize the result.
    let result_val = crate::eval::materialize(&result_thunk, None, &ctx)
        .await
        .map_err(|e| {
            TypeDiagnostic::error(
                "type-error",
                format!("type-stage materialization failed: {e}"),
                origin_span.clone(),
            )
        })?;

    // Convert TypeNode Value → Type.
    // If the direct structural conversion fails (unrecognised TypeNode tag), try
    // as_type_dispatch to normalise the value through the prelude's `as-typenode`
    // function before giving up.
    if let Some(ty) = typenode_value_to_type(&result_val, &ctx).await {
        return Ok(ty);
    }
    if let Some(ty) = as_type_dispatch(&result_val, state).await {
        return Ok(ty);
    }
    Err(TypeDiagnostic::error(
        "type-error",
        format!("type-stage result cannot be converted to Type: {result_val}"),
        origin_span,
    ))
}

/// Dispatch a TypeNode `Value` through the type-stage `as-typenode` protocol function.
///
/// This is the T-1059 hook for normalizing TypeNode values produced by type-stage
/// expression evaluation using user-defined `as-type:` annotations on TypeNode
/// constructors.  The name `as-typenode` is a **protocol contract** (D-7): Rust requires
/// this exact name in the type-stage map; the prelude is responsible for providing it.
/// This is not a prelude-specific hack — any compliant prelude must export a function
/// named `as-typenode` that accepts a TypeNode value and returns a resolved Type.
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
/// 1. Looks up `as-typenode` in `state.type_stage_scope` as a
///    `TypeStageEntry::Function(thunk_id)`.
/// 2. Materialises the thunk via `state.eval_ctx` to obtain the resolver function value.
/// 3. Calls `eval_type_stage_value(fn_val, &[val.clone()], state)` — the function value
///    receives the TypeNode value as its sole argument and returns a normalised TypeNode.
/// 4. Converts the normalised TypeNode value to a `Type` via `typenode_value_to_type`.
///
/// Returns `None` if:
/// - `state.type_stage_scope` is empty or does not contain `as-typenode`.
/// - The `as-typenode` entry is `Resolved` (not a function).
/// - `state.eval_ctx` is `None`.
/// - Thunk materialisation fails.
/// - `eval_type_stage_value` returns an error.
/// - The result of `eval_type_stage_value` is itself unrecognised (would cause recursion;
///   stopped at this depth).
///
/// ## Protocol contract (Axiom 1 / D-7)
///
/// `as-typenode` is an Axiom 1 protocol entry — the Rust type checker requires that the
/// active prelude provides a function named `as-typenode` that accepts TypeNode values and
/// returns their resolved Type.  This is analogous to `tmpl`/`unindent` for strings (D-3):
/// Rust defines the protocol; the prelude implements it.  A custom prelude must provide an
/// `as-typenode` function under this exact name to participate in TypeNode dispatch.
/// Tracked as decision D-7.
pub(crate) async fn as_type_dispatch(val: &Value, state: &mut InferState) -> Option<Type> {
    // Step 1: locate the `as-typenode` resolver in the type-stage scope chain.
    let resolver_thunk = {
        let mut found = None;
        for scope in &state.type_stage_scope {
            if let Some(entry) = scope.get("as-typenode") {
                match entry {
                    crate::type_infer::TypeStageEntry::Function(t) => {
                        found = Some(Arc::clone(t));
                        break;
                    }
                    // Resolved entry is a ground Type, not a function — cannot call it.
                    crate::type_infer::TypeStageEntry::Resolved(_) => return None,
                    _ => continue,
                }
            }
        }
        match found {
            Some(t) => t,
            None => return None,
        }
    };

    // Step 2: materialise the thunk to get the resolver function value.
    let eval_ctx = state.eval_ctx.clone()?;
    let fn_val = crate::eval::materialize(&resolver_thunk, None, &eval_ctx)
        .await
        .ok()?;

    // Step 3: call the resolver with the TypeNode value as its sole argument.
    // We use evaluate_resolver_by_value (inline) rather than eval_type_stage_value so that the
    // result conversion uses typenode_value_to_type ONLY (no as_type_dispatch fallback),
    // preventing infinite recursion if the resolver itself returns an unrecognised tag.
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
                return None; // as-typenode must be a single-parameter function
            }
            let call_ctx = crate::eval_call::CallContext {
                params,
                body,
                closure_env: Arc::clone(closure_env),
                positional: &[arg_thunk],
                named: None,
                default_env_id: 0,
                call_span: origin_span.clone(),
                ctx: &eval_ctx,
            };
            crate::eval_call::invoke_function(&call_ctx).await.ok()?
        }
        _ => return None,
    };

    let result_val = crate::eval::materialize(&result_thunk, None, &eval_ctx)
        .await
        .ok()?;

    // Convert using typenode_value_to_type ONLY — no as_type_dispatch fallback here.
    // This prevents infinite recursion: if the resolver itself returns an unrecognised tag,
    // we return None rather than attempting further dispatch.
    typenode_value_to_type(&result_val, &eval_ctx).await
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
/// In the fallback call sites (`resolve_property_dict_as_record`), errors are caught with
/// `.unwrap_or(Type::Unknown)`, preserving existing gradual-typing behaviour for
/// annotations that cannot be resolved to a precise type.
pub(crate) async fn eval_type_stage_expr(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
) -> Result<Type, TypeDiagnostic> {
    let node_span = node.span.clone();

    // Build a minimal EvalContext for evaluating the annotation node.
    // AMBIENT-OK: type-stage evaluation performs no file I/O.
    #[allow(clippy::disallowed_methods)]
    let base_dir =
        cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).map_err(|e| {
            TypeDiagnostic::error(
                "type-error",
                format!("type-stage eval: cannot open ambient dir: {e}"),
                node_span.clone(),
            )
        })?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, false);

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
    let core_thunk = Arc::new(Thunk::core_expr(
        Arc::new(lowered),
        crate::value::EvalFrame::empty(), // root scope
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
    if let Some(ty) = typenode_value_to_type(&typenode_val, &ctx).await {
        return Ok(ty);
    }
    if let Some(ty) = as_type_dispatch(&typenode_val, state).await {
        return Ok(ty);
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

/// An entry on the expansion stack used by `expand_named` / `expand_all_tycon_apps`.
///
/// Each entry carries a cloned `Arc<TyConDef>` (preserving pointer identity for
/// `Arc::ptr_eq` cycle detection) and the pre-assigned binder name that will be
/// used if a recursive reference to this entry is found during expansion.
///
/// The binder name is generated via `gensym_fresh('𝜇', alias_name)` before the body
/// is expanded, so that any self-reference in the body can return the pre-assigned name
/// rather than an uninitialized one.
pub(crate) type ExpansionStack = Vec<(Arc<crate::type_def::TyConDef>, String)>;

/// Returns `true` if `ty` contains any bare (unexpanded) type constructor references.
///
/// A "bare" type constructor reference is one of:
/// - `Type::TyCon(name)` — a named type constructor waiting to be expanded
/// - `Type::App(Type::TyCon(_), _)` — a type constructor application
/// - Any nested occurrence of the above
///
/// Used by `expand_named` Step 2b as a fast-path: if the alias body has no TyCon
/// references, it can be returned directly without further expansion.
pub(crate) fn body_contains_tycon_ref(ty: &Type) -> bool {
    match ty {
        Type::TyCon(_) => true,
        Type::App(f, arg) => body_contains_tycon_ref(f) || body_contains_tycon_ref(arg),
        Type::Dict(row) => {
            if row.fields.values().any(body_contains_tycon_ref) {
                return true;
            }
            if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    if body_contains_tycon_ref(k) {
                        return true;
                    }
                }
                body_contains_tycon_ref(value)
            } else {
                false
            }
        }
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, p)| body_contains_tycon_ref(p)) || body_contains_tycon_ref(ret)
        }
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().any(body_contains_tycon_ref)
        }
        Type::Negation(inner) => body_contains_tycon_ref(inner),
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => {
            if fields.fields.values().any(body_contains_tycon_ref) {
                return true;
            }
            if let crate::type_def::RowTail::Uniform { key, value } = &fields.tail {
                if let Some(k) = key {
                    if body_contains_tycon_ref(k) {
                        return true;
                    }
                }
                body_contains_tycon_ref(value)
            } else {
                false
            }
        }
        // S-860: equirecursive-types-core — a Recursive body may contain unexpanded TyCon refs.
        Type::Recursive { body, .. } => body_contains_tycon_ref(body),
        _ => false,
    }
}

/// Returns `true` if `ty` contains a `TypeVar` with the given name.
///
/// Used by `expand_named` Step 8 to determine whether the expanded body actually
/// contains a self-reference (making this a genuinely recursive type). If the
/// binder variable does not appear in the expanded body, the type is non-recursive
/// and no `Recursive` wrapper is needed.
///
/// Note: `TypeVar(var, 0)` at recursive positions is the sentinel produced by Step 4
/// (cycle detection). After expansion, `contains_recvar(expanded, binder_name)` checks
/// whether this sentinel appears anywhere in the body — confirming the alias is recursive.
pub(crate) fn contains_recvar(ty: &Type, var: &str) -> bool {
    match ty {
        Type::TypeVar(name, _) => name == var,
        Type::App(f, arg) => contains_recvar(f, var) || contains_recvar(arg, var),
        Type::Dict(row) => {
            if row.fields.values().any(|t| contains_recvar(t, var)) {
                return true;
            }
            if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    if contains_recvar(k, var) {
                        return true;
                    }
                }
                contains_recvar(value, var)
            } else {
                false
            }
        }
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, p)| contains_recvar(p, var)) || contains_recvar(ret, var)
        }
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().any(|m| contains_recvar(m, var))
        }
        Type::Negation(inner) => contains_recvar(inner, var),
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => {
            if fields.fields.values().any(|t| contains_recvar(t, var)) {
                return true;
            }
            if let crate::type_def::RowTail::Uniform { key, value } = &fields.tail {
                if let Some(k) = key {
                    if contains_recvar(k, var) {
                        return true;
                    }
                }
                contains_recvar(value, var)
            } else {
                false
            }
        }
        // S-860: equirecursive-types-core — recurse into the body of a nested Recursive.
        // Globally unique gensym var names guarantee no shadowing: an inner Recursive will
        // never carry the same `var` name as an outer one, so recursion is always safe.
        Type::Recursive { body, .. } => contains_recvar(body, var),
        _ => false,
    }
}

/// Returns `true` if `ty` is contractive with respect to `var`.
///
/// A type is contractive if it does NOT permit a bare self-reference to appear at the root
/// or inside a Union/Intersection without a guarding constructor. This ensures that recursive
/// types are well-founded and can be unfolded finitely.
///
/// **Contractiveness rules:**
/// 1. `TypeVar(var, _)` where `var` matches → NOT contractive (bare self-ref)
/// 2. `Union(members)` or `Intersection(members)` → ALL members must be contractive
/// 3. All other forms (Record, Function, App, TyCon, etc.) → contractive (guarding constructors)
///
/// Used by `expand_named` (T-1162) to verify that a recursive type alias body is well-formed
/// before wrapping it in `Type::Recursive`. Non-contractive types are infinite without a
/// guarding constructor and cannot be soundly expanded.
///
/// Example:
/// - `type Bad a = a` → NOT contractive (bare TypeVar)
/// - `type Bad a = [A a | B a]` → contractive (Union with guarding constructors A/B)
/// - `type Bad a = [a | a]` → NOT contractive (Union of bare self-refs)
/// - `type Good = [x: Int next: Good]` → contractive (Record guarding constructor)
pub(crate) fn is_contractive_type(ty: &Type, var: &str) -> bool {
    match ty {
        // Rule 1: Bare self-reference → NOT contractive.
        Type::TypeVar(name, _) if name == var => false,

        // Rule 2: Union/Intersection → ALL members must be contractive.
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().all(|m| is_contractive_type(m, var))
        }

        // Rule 3: All other forms are guarding constructors → contractive.
        // This includes:
        // - Record (structural guard)
        // - Function (arrow constructor)
        // - App (type application)
        // - TyCon (named type constructor)
        // - NominalVariant (variant constructor)
        // - Negation (type operator)
        // - Recursive (nested recursive type)
        // - Primitives (Int, Float, Str, Bool, etc.)
        _ => true,
    }
}

/// Expand a named type alias to its fully-expanded `Type`.
///
/// Implements the 6-step algorithm from the equirecursive-types design (§Annotation Resolver):
///
/// 1. Look up the `TyConDef` for `name` from `state.tycon_env`.
/// 2. Fast path: if the body is a primitive type and contains no `TyCon` references,
///    return the body directly (no expansion needed).
/// 3. Builtin-opaque path: if `builtin_type` is `Some`, return
///    `Type::App(Type::TyCon(name), args[0])` (or `Type::TyCon(name)` for zero-arg)
///    without structural expansion.
/// 4. Cycle detection: if the expansion stack already contains this `Arc<TyConDef>`
///    (via `Arc::ptr_eq`), this is a recursive reference. Return a `TypeVar` with
///    the pre-assigned binder name from the stack entry.
/// 5. Param substitution: build a `HashMap<String, Type>` from `params` → `args` and
///    apply it to the body via `apply_type_alias_substitution`.
/// 6. Recursive expansion: push a new stack entry (with a fresh gensym'd binder name),
///    call `expand_all_tycon_apps` on the substituted body, then pop.
///
/// Returns `None` if the name is not registered in `state.tycon_env`.
///
/// **Cycle origin detection (wrap rule):** after expansion, if the expanded
/// body contains a `TypeVar` matching the pre-assigned binder name, this alias IS the
/// cycle origin — the body is wrapped in `Type::Recursive { var: binder_name, body }`.
/// Non-recursive aliases are returned as-is (no wrapper needed).
///
/// The `Type::Recursive` produced here is consumed by `is_subtype` via the S-Exp + S-Assum
/// coinductive algorithm implemented in S-861. `expand_named` is wired into the annotation
/// resolver via `resolve_type_head` (S-862 complete).
pub(crate) fn expand_named(name: &str, args: &[Type], state: &mut InferState) -> Option<Type> {
    // Step 1: look up the TyConDef.
    // Lookup in state.tycon_env — the authoritative flat store, populated by typecheck_cek.rs
    // Pass 2 and builtins registration.
    let def_arc = state.tycon_env.get(name).cloned()?;
    let def = Arc::clone(&def_arc);

    // Arity check: number of args must match declared params.
    if args.len() != def.params.len() {
        // Arity mismatch — cannot expand. Fall back to a TyCon application so that
        // callers can still produce a type error message. Build the App chain.
        let base = Type::TyCon(name.to_string());
        if args.is_empty() {
            return Some(base);
        }
        let mut result = base;
        for arg in args {
            result = Type::App(Box::new(result), Box::new(arg.clone()));
        }
        return Some(result);
    }

    // Step 2a: nominal ADT guard — do NOT expand the body of a declared nominal type.
    // Nominal ADTs (those with declared constructors) must stay as TyCon/TyConResolved so that
    // nominal identity is preserved. Expanding a TyCon to its Union of constructors collapses
    // it into structural equivalence with any union that happens to match the body —
    // which is wrong for nominal typing. UNIFY-TYCON-EXPAND handles the TyCon ~
    // NominalVariant and TyCon ~ Union cases correctly without body expansion.
    if !def.constructors.is_empty() {
        let base = Type::TyConResolved(name.to_string(), def_arc);
        if args.is_empty() {
            return Some(base);
        }
        let mut result = base;
        for arg in args {
            result = Type::App(Box::new(result), Box::new(arg.clone()));
        }
        return Some(result);
    }

    // Step 2b: fast path for zero-param types with no TyCon references in body.
    // Structural aliases (Int, Float, etc.) have no params, no constructors, and no TyCon
    // references in body — return the body directly.
    if def.params.is_empty() && !body_contains_tycon_ref(&def.body) {
        return Some(def.body.clone());
    }

    // Step 3: builtin-opaque types — do not structurally expand.
    // Return App(TyConResolved(name, arc), args) so that UNIFY-TYCON handles them by Arc identity
    // and variance-directed comparison, not by structural equivalence.
    if def.builtin_type.is_some() {
        let base = Type::TyConResolved(name.to_string(), def_arc);
        if args.is_empty() {
            return Some(base);
        }
        let mut result = base;
        for arg in args {
            result = Type::App(Box::new(result), Box::new(arg.clone()));
        }
        return Some(result);
    }

    // Step 4: cycle detection via Arc::ptr_eq.
    // If the same Arc<TyConDef> is already on the stack, this is a self-referential
    // (recursive) type. Return a TypeVar with the pre-assigned binder name so that the
    // caller receives a stable type identity for the recursive position.
    for (stack_def, binder_name) in state.expansion_stack.iter() {
        if Arc::ptr_eq(stack_def, &def) {
            return Some(Type::TypeVar(binder_name.clone(), 0));
        }
    }

    // Step 5: param substitution.
    // Build a HashMap from parameter name → concrete arg type and apply it to the body.
    // Uses `apply_type_alias_substitution` which handles all Type variants including
    // TypeVar (for parameter tokens), Record, Function, Union, App, etc.
    let body_substituted = if def.params.is_empty() {
        def.body.clone()
    } else {
        let mut param_subst: HashMap<String, Type> = HashMap::new();
        for (param, arg) in def.params.iter().zip(args.iter()) {
            param_subst.insert(param.clone(), arg.clone());
        }
        apply_type_alias_substitution(&def.body, &param_subst, state)
    };

    // Step 6: push to stack with a fresh binder name, expand, then pop.
    // The binder name is pre-assigned BEFORE expansion so that Step 4 above can return
    // it when the body self-references this alias (S-860: equirecursive-types-core).
    //
    // The gensym_fresh call is here — BEFORE stack.push — so the pre-assigned name is
    // available to Step 4 (cycle detection) for any self-reference in the body.
    let binder_name = crate::builtins_meta::gensym_fresh('𝜇', name);
    state.expansion_stack.push((def, binder_name.clone()));

    let expanded = expand_all_tycon_apps(&body_substituted, state);

    state.expansion_stack.pop();

    // Recursive wrapping rule (Step 6 wrap — S-860: equirecursive-types-core).
    //
    // Check whether the pre-assigned binder name appears in the expanded body as a
    // TypeVar sentinel (produced by Step 4 cycle detection when the body self-references
    // this alias). If yes, this alias IS recursive — wrap the body in `Type::Recursive`.
    // If no (non-recursive alias), return the body as-is.
    //
    // This wrapping rule implements §Annotation Resolver Path 1 from
    // doc/whatif/equirecursive-types.md: "wrap `Recursive` only when popping the stack
    // entry whose fresh name appears in the expanded body."
    //
    // The S-Exp + S-Assum coinductive subtype algorithm that CONSUMES `Type::Recursive`
    // was implemented in S-861 (`is_atom_subtype` in src/bas.rs). `expand_named` is wired
    // into the annotation resolver via `resolve_type_head` (S-862 complete).
    if contains_recvar(&expanded, &binder_name) {
        // T-1162: Contractiveness check BEFORE wrapping in Type::Recursive.
        // A recursive type must be contractive — the self-reference must appear under a
        // guarding constructor (Record, Function, App, etc.), NOT as a bare TypeVar or
        // inside a Union/Intersection of bare self-refs.
        //
        // Non-contractive types are infinite without a guarding constructor and cannot be
        // soundly expanded. Example: `type Bad a = a` → infinite regress.
        //
        // If the expanded body is NOT contractive, return it as-is WITHOUT wrapping.
        // This lets the caller handle the non-recursive (or malformed) type downstream.
        if !is_contractive_type(&expanded, &binder_name) {
            // Non-contractive recursive type — do NOT wrap in Recursive.
            // Return the expanded body as-is. Downstream type checking will likely produce
            // an error when it encounters the bare TypeVar sentinel.
            return Some(expanded);
        }

        // Recursive alias: wrap in Type::Recursive with the pre-assigned binder name.
        // TypeVar(binder_name, 0) in `expanded` marks the recursive positions (self-refs).
        Some(Type::Recursive {
            var: binder_name,
            body: Box::new(expanded),
        })
    } else {
        // Non-recursive alias: return the expanded body as-is (no wrapper needed).
        Some(expanded)
    }
}

/// Recursively expand all `Type::TyCon` and `Type::App(TyCon, _)` references in `ty`.
///
/// This is the structural walker that drives alias expansion. It calls `expand_named`
/// for each named type constructor it encounters, passing the current expansion stack
/// for cycle detection.
///
/// - `Type::TyCon(name)` → `expand_named(name, &[], stack, state)`
/// - `Type::App(Type::TyCon(name), arg)` → expand arg first, then
///   `expand_named(name, &[expanded_arg], stack, state)`
/// - Nested `Type::App(Type::App(TyCon, a), b)` chains → collect all args in order,
///   call `expand_named(name, &[a, b, ...], stack, state)` (curried left-assoc)
/// - All other type forms → recurse structurally into children
///
/// Returns the expanded type. Falls back to the original type node when expansion
/// fails (e.g., unknown alias name — the TyCon is preserved for downstream error
/// reporting).
pub(crate) fn expand_all_tycon_apps(ty: &Type, state: &mut InferState) -> Type {
    match ty {
        // Bare TyCon — zero-arg application.
        Type::TyCon(name) => expand_named(name, &[], state).unwrap_or_else(|| ty.clone()),

        // App chain: collect the root TyCon name and all args, then expand.
        // App is left-associative: App(App(TyCon("Map"), Str), Int) = Map[Str][Int].
        // We collect args right-to-left while peeling left-associative Apps, then reverse.
        Type::App(f, arg) => {
            // Expand the argument first (args are always expanded before the ctor).
            let expanded_arg = expand_all_tycon_apps(arg, state);

            // Check whether `f` is a TyCon or another App(TyCon, ...) chain.
            // Collect the root name and all preceding args by peeling the App spine.
            let (root_name, preceding_args) = collect_app_spine(f);

            match root_name {
                Some(name) => {
                    // Expand preceding args first, then append the current expanded_arg.
                    let mut all_args: Vec<Type> = preceding_args
                        .iter()
                        .map(|a| expand_all_tycon_apps(a, state))
                        .collect();
                    all_args.push(expanded_arg);

                    expand_named(name, &all_args, state).unwrap_or_else(|| {
                        // Unknown alias — rebuild the App chain with the expanded args.
                        let base = Type::TyCon(name.to_string());
                        all_args
                            .into_iter()
                            .fold(base, |acc, a| Type::App(Box::new(acc), Box::new(a)))
                    })
                }
                None => {
                    // `f` is not a TyCon chain (e.g., App(TypeVar, arg)) — recurse into f.
                    let expanded_f = expand_all_tycon_apps(f, state);
                    Type::App(Box::new(expanded_f), Box::new(expanded_arg))
                }
            }
        }

        // Structural recursion for all other type forms.
        Type::Dict(row) => {
            let new_fields: indexmap::IndexMap<String, Type> = row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, state)))
                .collect();
            let new_tail = match &row.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, state))),
                        value: Box::new(expand_all_tycon_apps(value, state)),
                    }
                }
                other => other.clone(),
            };
            Type::Dict(Row {
                fields: new_fields,
                tail: new_tail,
            })
        }

        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, p)| (name.clone(), expand_all_tycon_apps(p, state)))
                .collect(),
            typed_variadics: typed_variadics
                .iter()
                .map(|(name, p)| (name.clone(), expand_all_tycon_apps(p, state)))
                .collect(),
            rest: rest
                .as_ref()
                .map(|boxed| Box::new((boxed.0.clone(), expand_all_tycon_apps(&boxed.1, state)))),
            ret: Box::new(expand_all_tycon_apps(ret, state)),
            required_count: *required_count,
        },

        Type::Union(members) => Type::normalize_union(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, state))
                .collect(),
        ),

        Type::Intersection(members) => Type::normalize_intersection(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, state))
                .collect(),
        ),

        Type::Negation(inner) => Type::Negation(Box::new(expand_all_tycon_apps(inner, state))),

        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => {
            let new_fields: indexmap::IndexMap<String, Type> = fields
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, state)))
                .collect();
            let new_tail = match &fields.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, state))),
                        value: Box::new(expand_all_tycon_apps(value, state)),
                    }
                }
                other => other.clone(),
            };
            Type::NominalVariant {
                tycon: tycon.clone(),
                ctor: ctor.clone(),
                fields: Row {
                    fields: new_fields,
                    tail: new_tail,
                },
            }
        }

        // S-860: equirecursive-types-core — recurse into the body of a Recursive type.
        // A Recursive wrapper (produced by expand_named's Step 8 on an earlier pass) may
        // contain TyCon references in its body that still need expansion. The `var` binder
        // is preserved unchanged — it is a μ-binder name, not a type alias reference.
        Type::Recursive { var, body } => Type::Recursive {
            var: var.clone(),
            body: Box::new(expand_all_tycon_apps(body, state)),
        },

        // Atomic types (primitives, TypeVar, Unknown, Top, etc.) — no TyCon children.
        _ => ty.clone(),
    }
}

/// Collect the root `TyCon` name and all intermediate argument types from a left-associative
/// `App` spine.
///
/// Given `App(App(TyCon("Map"), Str), Int)`, returns `(Some("Map"), [Str, Int])`.
/// Given `App(TypeVar("a"), Int)`, returns `(None, [])` — no TyCon root.
///
/// The returned `preceding_args` are in left-to-right order (first arg first).
fn collect_app_spine(ty: &Type) -> (Option<&str>, Vec<&Type>) {
    match ty {
        Type::TyCon(name) => (Some(name.as_str()), vec![]),
        Type::App(f, arg) => {
            let (root, mut args) = collect_app_spine(f);
            if root.is_some() {
                args.push(arg.as_ref());
            }
            (root, args)
        }
        _ => (None, vec![]),
    }
}

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
#[allow(clippy::too_many_arguments)]
async fn try_resolve_fn_type_expr(
    entries: &[Spanned<SurfaceEntry>],
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Option<Type>, TypeDiagnostic> {
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

    let required_count = params.len();
    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
        typed_variadics: vec![],
        rest: None,
        required_count,
    }))
}
