//! Type annotation resolution and type expression parsing.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::rust_span;
use crate::type_class::ConstraintArg;
use crate::type_def::Variance;
use crate::type_errors::{GenericTypeError, TypeErrorTyped, UndefinedType};
use crate::types::{Constraint, InferState, Kind, Row, Type, TypeAlias, TypeEnv, TypeError};
use crate::value::{HashableValue, Thunk, Value};

/// Convert a variance annotation name to a `Variance` value (T-953).
///
/// Used in `[let a@Covariant b@Contravariant c]` type parameter processing:
/// before checking if the annotation is a typeclass name in ClassEnv, call
/// this function to handle variance annotations first.
///
/// Returns `Some(v)` for known variance names, `None` for everything else
/// (which is then checked against ClassEnv as a typeclass constraint).
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
/// `params` are the FRESH TypeVar names (e.g., `_t0`, `_t1`) that params were remapped to.
/// Called after alias body resolution so we operate on real `Type` values.
pub(crate) fn infer_variance(body: &Type, params: &[String], type_env: &TypeEnv) -> Vec<Variance> {
    let n = params.len();
    let mut pos_seen = vec![false; n];
    let mut neg_seen = vec![false; n];

    walk_polarity(
        body,
        Polarity::Positive,
        params,
        &mut pos_seen,
        &mut neg_seen,
        type_env,
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
    type_env: &TypeEnv,
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
        Type::Record(row) => {
            // Record fields are in covariant (positive) position.
            for t in row.fields.values() {
                walk_polarity(t, pol, params, pos_seen, neg_seen, type_env);
            }
            // Uniform tail key and value types also in covariant position.
            // The key type can be a TypeVar (e.g. `[type [let k v] {_@k: v}]`), so both must
            // be visited. Mirrors the B-328 fix in type_unify.rs (lower_levels_check_occurs).
            if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    walk_polarity(k, pol, params, pos_seen, neg_seen, type_env);
                }
                walk_polarity(value, pol, params, pos_seen, neg_seen, type_env);
            }
        }
        Type::Function {
            params: fn_params,
            ret,
            ..
        } => {
            // Function parameters are contravariant (flip polarity).
            for (_, pt) in fn_params {
                walk_polarity(pt, pol.flip(), params, pos_seen, neg_seen, type_env);
            }
            // Return type is covariant.
            walk_polarity(ret, pol, params, pos_seen, neg_seen, type_env);
        }
        Type::App(f, arg) => {
            // Check if f is a TyCon with known variance for the argument.
            if let Type::TyCon(tcon_name) = f.as_ref() {
                if let Some(def) = type_env.lookup_tycon_def(tcon_name) {
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
                                    type_env,
                                );
                                walk_polarity(
                                    arg,
                                    Polarity::Negative,
                                    params,
                                    pos_seen,
                                    neg_seen,
                                    type_env,
                                );
                                return;
                            }
                            Variance::Phantom => return, // Phantom: argument is not used.
                        };
                        walk_polarity(arg, effective_pol, params, pos_seen, neg_seen, type_env);
                        return;
                    }
                }
            }
            // Unknown constructor or multi-arg App(App(..)) chain — conservative: treat as invariant.
            // Walk both f and arg so that TypeVars inside f (e.g., App(App(TyCon("Map"), a), b)
            // where f = App(TyCon("Map"), a)) are visited and do not register as Phantom.
            walk_polarity(f, Polarity::Positive, params, pos_seen, neg_seen, type_env);
            walk_polarity(f, Polarity::Negative, params, pos_seen, neg_seen, type_env);
            walk_polarity(
                arg,
                Polarity::Positive,
                params,
                pos_seen,
                neg_seen,
                type_env,
            );
            walk_polarity(
                arg,
                Polarity::Negative,
                params,
                pos_seen,
                neg_seen,
                type_env,
            );
        }
        Type::Union(members) | Type::Intersection(members) => {
            // Union/Intersection members preserve the current polarity (join/meet).
            for m in members {
                walk_polarity(m, pol, params, pos_seen, neg_seen, type_env);
            }
        }
        Type::Negation(inner) => {
            // Negation flips polarity.
            walk_polarity(inner, pol.flip(), params, pos_seen, neg_seen, type_env);
        }
        // NominalVariant fields are in covariant position — values stored in a variant
        // constructor are accessible (read), so they vary covariantly.
        // This ensures that `a` in `Result[Ok value: a]` is not classified as Phantom.
        Type::NominalVariant { fields, .. } => {
            for t in fields.fields.values() {
                walk_polarity(t, pol, params, pos_seen, neg_seen, type_env);
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
                    walk_polarity(k, pol, params, pos_seen, neg_seen, type_env);
                }
                walk_polarity(value_ty, pol, params, pos_seen, neg_seen, type_env);
            }
        }
        // S-860: equirecursive-types-core — recurse into the body.
        // The `var` binder is the μ-binder name, not a type parameter; only the body is walked.
        // Variance of type parameters inside the body is unchanged by the μ-binder.
        Type::Recursive { var: _, body } => {
            walk_polarity(body, pol, params, pos_seen, neg_seen, type_env);
        }
        // Concrete types (Int, Str, Bool, etc.), TyCon, Unknown, Top, Error — no TypeVar involvement.
        _ => {}
    }
}

pub(crate) async fn expand_type_alias(
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
    let mut _alias_expand_constraints: Vec<Constraint> = Vec::new();
    let _ = resolve_type_expr(
        inner,
        env,
        state,
        &mut _alias_expand_constraints,
        &mut Some(&mut alias_ann_map),
        &mut None,
        None,
    )
    .await?;
    Ok(Type::Unknown)
}

pub(crate) async fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
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
        constraints,
        &mut ann_mapping_opt,
        &mut row_ann_mapping_opt,
        None,
    )
    .await
    .map_err(|e| vec![e])?;

    // Use checking mode for TypeAssert inner expression (doc/06 §Bidirectional Typing).
    let check_result =
        check_surface_expr(inner, &expected, env, state, constraints, type_map).await;

    // If checking fails, propagate errors (TypeAssert failures are hard type errors).
    if let Err(type_errors) = check_result {
        let has_default = annotation.node.get_property("default").is_some();
        if !has_default {
            return Err(type_errors);
        }
    }

    // Validate the default value type — hard error if the default cannot satisfy the asserted type.
    if let Some(default_node) = annotation.node.get_property("default") {
        match infer_surface_expr(default_node, env, state, constraints, type_map).await {
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
                let passes =
                    Type::is_subtype(&default_ty, &expected_resolved, Some(&state.tycon_env))
                        || ((contains_unknown_or_top(&default_ty)
                            || contains_unknown_or_top(&expected_resolved))
                            && Type::is_consistent(&default_ty, &expected_resolved));
                if !passes {
                    return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "default value type mismatch: default has type {default_ty}, \
                             but assertion expects {expected_resolved}"
                        ),
                        span: default_node.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    })]);
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
                return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "invalid repr: \"{repr_val}\" — must be one of: {}",
                        VALID_REPRS.join(", ")
                    ),
                    span: repr_node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })]);
            }
            // Check consistency: repr requires a numeric type (Int or Float)
            let is_numeric = matches!(&expected, Type::Int | Type::Float);
            if !is_numeric {
                return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "repr: \"{repr_val}\" requires a numeric type, but annotation declares {}",
                        expected
                    ),
                    span: repr_node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })]);
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
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    if name == "Fn" {
        resolve_fn_type(
            &annotation.node,
            env,
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
            env,
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
            env,
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
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
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: "bind: list must contain only positional entries (bare names)".to_string(),
                                        span: bind_entry.span.clone(),
                                        notes: vec![], call_stack: vec![],
                                    }));
                                }
                                match &bind_entry.node.value.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        // Check lowercase convention for TypeVar names
                                        if !name.starts_with(|c: char| c.is_lowercase()) {
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: format!(
                                                    "bind: TypeVar name '{}' must start with lowercase letter",
                                                    name
                                                ),
                                                span: bind_entry.node.value.span.clone(),
                                                notes: vec![], call_stack: vec![],
                                            }));
                                        }
                                        // Create fresh TypeVar and register in ann_mapping
                                        let n = state.subst.name_counter.get();
                                        let fresh = format!("_t{}", n);
                                        state.subst.name_counter.set(n.saturating_add(1));
                                        state.levels.insert(fresh.clone(), state.level);
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), name.clone());
                                        if let Some(ref mut mapping) = ann_mapping {
                                            mapping.insert(name.clone(), fresh);
                                        } else {
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: "bind: requires an annotation mapping context".to_string(),
                                                span,
                                                notes: vec![], call_stack: vec![],
                                            }));
                                        }
                                    }
                                    _ => {
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message:
                                                "bind: entries must be bare names (TypeVar names)"
                                                    .to_string(),
                                            span: bind_entry.node.value.span.clone(),
                                            notes: vec![],
                                            call_stack: vec![],
                                        }));
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
                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                    message: "bind: list must contain only bare names, not named arguments".to_string(),
                                    span: entry.node.value.span.clone(),
                                    notes: vec![], call_stack: vec![],
                                }));
                            }
                            // Collect all names: func first, then each positional arg
                            let all_names: Vec<(&str, Span)> = {
                                let mut v: Vec<(&str, Span)> = Vec::new();
                                match &func.expr {
                                    SurfaceExpression::VarRef { name, .. } => {
                                        v.push((name.as_str(), func.span.clone()))
                                    }
                                    _ => {
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message:
                                                "bind: entries must be bare names (TypeVar names)"
                                                    .to_string(),
                                            span: func.span.clone(),
                                            notes: vec![],
                                            call_stack: vec![],
                                        }))
                                    }
                                }
                                for arg in args.iter() {
                                    match &arg.expr {
                                            SurfaceExpression::VarRef { name, .. } => {
                                                v.push((name.as_str(), arg.span.clone()))
                                            }
                                            _ => return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: "bind: entries must be bare names (TypeVar names)".to_string(),
                                                span: arg.span.clone(),
                                                notes: vec![], call_stack: vec![],
                                            })),
                                        }
                                }
                                v
                            };
                            for (name, name_span) in all_names {
                                if !name.starts_with(|c: char| c.is_lowercase()) {
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: format!(
                                            "bind: TypeVar name '{}' must start with lowercase letter",
                                            name
                                        ),
                                        span: name_span,
                                        notes: vec![], call_stack: vec![],
                                    }));
                                }
                                let n = state.subst.name_counter.get();
                                let fresh = format!("_t{}", n);
                                state.subst.name_counter.set(n.saturating_add(1));
                                state.levels.insert(fresh.clone(), state.level);
                                // Register source name for better T013 diagnostics
                                state
                                    .type_var_source_names
                                    .insert(fresh.clone(), name.to_string());
                                if let Some(ref mut mapping) = ann_mapping {
                                    mapping.insert(name.to_string(), fresh);
                                } else {
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: "bind: requires an annotation mapping context"
                                            .to_string(),
                                        span,
                                        notes: vec![],
                                        call_stack: vec![],
                                    }));
                                }
                            }
                        }
                        _ => {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: "bind: value must be a list [a b c]".to_string(),
                                span: entry.node.value.span.clone(),
                                notes: vec![],
                                call_stack: vec![],
                            }))
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
                                let typevar_name = match &kind_entry.node.key {
                                    Some(k) => match &k.expr {
                                        SurfaceExpression::Str(s) => s.clone(),
                                        _ => {
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message:
                                                    "kinds: keys must be bare words (TypeVar names)"
                                                        .to_string(),
                                                span: kind_entry.span.clone(),
                                                notes: vec![],
                                                call_stack: vec![],
                                            }))
                                        }
                                    },
                                    None => {
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message: "kinds: entries must be keyed [name: kind]"
                                                .to_string(),
                                            span: kind_entry.span.clone(),
                                            notes: vec![],
                                            call_stack: vec![],
                                        }))
                                    }
                                };

                                // Validate that this name was declared in bind:
                                let type_var = if let Some(ref mapping) = ann_mapping {
                                    match mapping.get(&typevar_name) {
                                        Some(var) => var.clone(),
                                        None => {
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: format!(
                                                    "kinds: TypeVar '{}' not found in bind: list",
                                                    typevar_name
                                                ),
                                                span: kind_entry.span.clone(),
                                                notes: vec![],
                                                call_stack: vec![],
                                            }))
                                        }
                                    }
                                } else {
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: "kinds: requires an annotation mapping context"
                                            .to_string(),
                                        span,
                                        notes: vec![],
                                        call_stack: vec![],
                                    }));
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
                                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                    message: format!(
                                                        "unknown kind '{}' (valid: Operator, Label)",
                                                        kind_name
                                                    ),
                                                    span: kind_entry.node.value.span.clone(),
                                                    notes: vec![], call_stack: vec![],
                                                }))
                                            }
                                        };
                                        state.kind_env.insert(type_var, kind);
                                    }
                                    _ => {
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message: "kinds: value must be a kind name (Operator or Label)".to_string(),
                                            span: kind_entry.node.value.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        }));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: "kinds: value must be a dict [name: kind ...]".to_string(),
                                span: entry.node.value.span.clone(),
                                notes: vec![],
                                call_stack: vec![],
                            }))
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
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: "constraint key must be a bare word (TypeVar name)".to_string(),
                                                span: c_entry.span.clone(),
                                                notes: vec![], call_stack: vec![],
                                            }));
                                        }
                                    },
                                    None => unreachable!(), // already checked above
                                };

                                // Create or get the TypeVar for this name
                                let type_var = if let Some(ref mut mapping) = ann_mapping {
                                    if let Some(existing_var) = mapping.get(&typevar_name) {
                                        existing_var.clone()
                                    } else {
                                        let n = state.subst.name_counter.get();
                                        let fresh = format!("_t{}", n);
                                        state.subst.name_counter.set(n.saturating_add(1));
                                        state.levels.insert(fresh.clone(), state.level);
                                        // Register source name for better T013 diagnostics
                                        state
                                            .type_var_source_names
                                            .insert(fresh.clone(), typevar_name.clone());
                                        mapping.insert(typevar_name.clone(), fresh.clone());
                                        fresh
                                    }
                                } else {
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: "constraint annotations require an annotation mapping context".to_string(),
                                        span,
                                        notes: vec![], call_stack: vec![],
                                    }));
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
                                            return Err(TypeErrorTyped::Generic(
                                                GenericTypeError {
                                                    message: format!(
                                                        "unknown constraint class '{}'",
                                                        name
                                                    ),
                                                    span: c_entry.node.value.span.clone(),
                                                    notes: vec![],
                                                    call_stack: vec![],
                                                },
                                            ));
                                        }
                                        state.add_constraint(
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
                                                    // [a: [each Comparable Showable]] — skip 'each'
                                                    &class_list[1..]
                                                } else {
                                                    // [a: [Comparable Showable]] — no 'each', error
                                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                        message: "constraint class list must start with 'each' keyword: use [each ClassName ...]".to_string(),
                                                        span: class_list[0].span.clone(),
                                                        notes: vec![], call_stack: vec![],
                                                    }));
                                                }
                                            } else {
                                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                    message: "constraint class list must start with 'each' keyword: use [each ClassName ...]".to_string(),
                                                    span: class_list[0].span.clone(),
                                                    notes: vec![], call_stack: vec![],
                                                }));
                                            }
                                        } else {
                                            return Err(TypeErrorTyped::Generic(
                                                GenericTypeError {
                                                    message:
                                                        "constraint class list cannot be empty"
                                                            .to_string(),
                                                    span: c_entry.node.value.span.clone(),
                                                    notes: vec![],
                                                    call_stack: vec![],
                                                },
                                            ));
                                        };

                                        // Multiple classes: iterate and add each
                                        for class_entry in class_entries {
                                            if class_entry.node.key.is_some() {
                                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                    message: "constraint class list must contain only positional entries".to_string(),
                                                    span: class_entry.span.clone(),
                                                    notes: vec![], call_stack: vec![],
                                                }));
                                            }
                                            match &class_entry.node.value.expr {
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    if !VALID_CLASSES.contains(&name.as_str())
                                                        && state.class_env.get(name).is_none()
                                                    {
                                                        return Err(TypeErrorTyped::Generic(
                                                            GenericTypeError {
                                                                message: format!(
                                                                    "unknown constraint class '{}'",
                                                                    name
                                                                ),
                                                                span: class_entry
                                                                    .node
                                                                    .value
                                                                    .span
                                                                    .clone(),
                                                                notes: vec![],
                                                                call_stack: vec![],
                                                            },
                                                        ));
                                                    }
                                                    state.add_constraint(
                                                        constraints,
                                                        name.clone(),
                                                        type_var.clone(),
                                                    );
                                                }
                                                _ => {
                                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                        message: "constraint class must be a class name (e.g., Comparable)".to_string(),
                                                        span: class_entry.node.value.span.clone(),
                                                        notes: vec![], call_stack: vec![],
                                                    }));
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
                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: "constraint class list must not contain named arguments".to_string(),
                                                span: c_entry.node.value.span.clone(),
                                                notes: vec![], call_stack: vec![],
                                            }));
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
                                                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                                    message: "constraint class must be a class name (e.g., Comparable)".to_string(),
                                                                    span: arg.span.clone(),
                                                                    notes: vec![], call_stack: vec![],
                                                                }))
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
                                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                        message: "constraint value must be a class name or [each Class1 Class2 ...]".to_string(),
                                                        span: c_entry.node.value.span.clone(),
                                                        notes: vec![], call_stack: vec![],
                                                    }))
                                                }
                                            };
                                        for (name, name_span) in class_names {
                                            if !VALID_CLASSES.contains(&name)
                                                && state.class_env.get(name).is_none()
                                            {
                                                return Err(TypeErrorTyped::Generic(
                                                    GenericTypeError {
                                                        message: format!(
                                                            "unknown constraint class '{}'",
                                                            name
                                                        ),
                                                        span: name_span,
                                                        notes: vec![],
                                                        call_stack: vec![],
                                                    },
                                                ));
                                            }
                                            state.add_constraint(
                                                constraints,
                                                name.to_string(),
                                                type_var.clone(),
                                            );
                                        }
                                    }
                                    _ => {
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message: "constraint value must be a class name or list of class names".to_string(),
                                            span: c_entry.node.value.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        }));
                                    }
                                }
                            }
                        }
                        _ => {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: "constraint: value must be a dict [a: Comparable]"
                                    .to_string(),
                                span: entry.node.value.span.clone(),
                                notes: vec![],
                                call_stack: vec![],
                            }));
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
                                                TypeErrorTyped::Generic(GenericTypeError {
                                                    message: format!(
                                                        "unknown class '{}' in MPTC constraint",
                                                        class_name
                                                    ),
                                                    span: c_entry.node.value.span.clone(),
                                                    notes: vec![],
                                                    call_stack: vec![],
                                                })
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
                                                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                                message: format!(
                                                                    "TypeVar '{}' not declared in bind: — add bind: [{}] before constraint:",
                                                                    var_name, var_name
                                                                ),
                                                                span: subsequent.node.value.span.clone(),
                                                                notes: vec![], call_stack: vec![],
                                                            }));
                                                        }
                                                        // Map to the actual TypeVar name (e.g., _t0)
                                                        let actual_var =
                                                            mapping.get(var_name).unwrap().clone();
                                                        typevar_names.push(actual_var);
                                                    } else {
                                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                            message: "constraint annotations require an annotation mapping context".to_string(),
                                                            span,
                                                            notes: vec![], call_stack: vec![],
                                                        }));
                                                    }
                                                }
                                                SurfaceExpression::VarRef {
                                                    escaped: true, ..
                                                } => {
                                                    // Another escaped ref — start of the next MPTC
                                                    break;
                                                }
                                                _ => {
                                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                        message: "MPTC constraint entries after class name must be TypeVar names".to_string(),
                                                        span: subsequent.node.value.span.clone(),
                                                        notes: vec![], call_stack: vec![],
                                                    }));
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
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message: "positional constraint entries must start with escaped class name (e.g., $Add)".to_string(),
                                            span: c_entry.node.value.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        }));
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
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: "doc: value must be a string literal".to_string(),
                                span: entry.node.value.span.clone(),
                                notes: vec![],
                                call_stack: vec![],
                            }));
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
            if let SurfaceExpression::Str(key_name) = &key_expr.expr {
                if !VALID_FN_ANNOTATION_KEYS.contains(&key_name.as_str()) {
                    state.diagnostics.push(crate::error::TypeDiagnostic {
                        message: format!(
                            "unknown function annotation key '{}' (valid keys: {})",
                            key_name,
                            VALID_FN_ANNOTATION_KEYS.join(", ")
                        ),
                        span: key_expr.span.clone(),
                        code: super::typecheck_diag::T021_UNKNOWN_TYPE_PARAM_ANNOTATION,
                        level: crate::error::DiagnosticLevel::Warn,
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
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
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "fn annotation must use either named keys (return:, constraint:, doc:, bind:, kinds:) or positional entries (union return type), not both".to_string(),
                        span,
                        notes: vec![], call_stack: vec![],
                    }));
                }
                let (ret, _doc) = resolve_fn_metadata(
                    surface_entries,
                    env,
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
                    variadic: false,
                    required_count: 0,
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
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await
            }
        }
        _ => {
            // Simple(name) path: fn@Int, fn@a, etc.
            let ret = resolve_annotation_as_type(
                ann,
                env,
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
                variadic: false,
                required_count: 0,
            };
            crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(
                name,
                env,
                span,
                state,
                constraints,
                ann_mapping,
                &row_ref,
                type_params_scope,
            )
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
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await
        }
        Annotation::Annotated(name, inner) => {
            // For fn annotations, forward to full resolver
            // (e.g., fn@Seq@Int should resolve the Annotated properly)
            resolve_annotation(
                &Annotation::Annotated(name.clone(), inner.clone()),
                env,
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => {
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(
                name,
                env,
                span,
                state,
                constraints,
                ann_mapping,
                &row_ref,
                type_params_scope,
            )
        }
        Annotation::Annotated(name, inner) => {
            // Parameterized type annotations: Seq@Int, Map@[String: Int], Record@[field: Type]
            // Note: "Seq" no longer has a special case — it resolves through TyCon lookup like
            // any other user-declared parameterized type (type-foundations S-894).
            match name.as_str() {
                "Map" => {
                    // Resolve the inner annotation for key and value types
                    match inner.as_ref() {
                        Annotation::Simple(_) => {
                            // @Map@T (single type) → Map[fresh_key: T]
                            // Use a fresh TypeVar for the key so callers can unify against
                            // concrete key types instead of being stuck with Unknown.
                            let value_type = Box::pin(resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            Ok(Type::map(state.fresh_type_var(), value_type))
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
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    )
                                    .await?
                                } else {
                                    Type::Unknown
                                };
                                let value_ty = resolve_type_expr(
                                    &v_entry.node.value,
                                    env,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                )
                                .await?;
                                Ok(Type::map(key_ty, value_ty))
                            } else {
                                // No "value:" key — delegate to resolve_type_dict which handles
                                // positional forms like [Map K V] (though nested inside @Map@).
                                Box::pin(resolve_type_dict(
                                    surface_entries,
                                    env,
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
                        _ => {
                            // Other forms like @Map@Annotated — treat as single value type.
                            // Use a fresh TypeVar for the key so callers can unify against
                            // concrete key types instead of being stuck with Unknown.
                            let value_type = Box::pin(resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            Ok(Type::map(state.fresh_type_var(), value_type))
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
                            Box::pin(resolve_type_dict(
                                surface_entries,
                                env,
                                span,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await
                        }
                        _ => Err(TypeErrorTyped::Generic(GenericTypeError {
                            message:
                                "Record parameterization requires a dict: @Record@[field: Type ...]"
                                    .to_string(),
                            span,
                            notes: vec![],
                            call_stack: vec![],
                        })),
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
                    let cap_type = Box::pin(resolve_annotation(
                        inner,
                        env,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await?;
                    Ok(Type::handle(cap_type))
                }
                _ => {
                    // Try TyConDef lookup for user-defined parameterized types (T-949).
                    // Handles `@Tree@Int` where Tree is a user-defined TyCon with arity 1.
                    if let Some(def) = env.lookup_tycon_def(name) {
                        if def.arity() >= 1 {
                            // Resolve the inner annotation as the first type argument.
                            let arg = Box::pin(resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            // Expand via expand_named; falls back to App(TyCon, arg) for
                            // builtins and ADTs (which expand_named returns as App chains).
                            return Ok(expand_named(name, &[arg.clone()], env, state)
                                .unwrap_or_else(|| {
                                    Type::App(Box::new(Type::TyCon(name.clone())), Box::new(arg))
                                }));
                        } else if def.arity() == 0 {
                            // Zero-arity TyCon with annotation — expand via expand_named.
                            return Ok(expand_named(name, &[], env, state)
                                .unwrap_or_else(|| Type::TyCon(name.clone())));
                        }
                    }
                    // Unknown parameterized type — no TyConDef found
                    Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!("unknown parameterized type: {}", name),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }))
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
                resolve_type_expr(
                    type_node,
                    env,
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
                    SurfaceExpression::Str(_) => Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "label: value must be a bare name (e.g. `label: l`), not a string literal".to_string(),
                        span,
                        notes: vec![], call_stack: vec![],
                    })),
                    SurfaceExpression::VarRef { name, .. } => {
                        if name.starts_with(|c: char| c.is_uppercase()) {
                            Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "label: value must be a lowercase type variable name (e.g. `label: l`), got '{}'",
                                    name
                                ),
                                span,
                                notes: vec![], call_stack: vec![],
                            }))
                        } else {
                            // Valid lowercase label name: create a Label-kinded TypeVar.
                            // If we're inside a function scope (ann_mapping is Some), reuse the
                            // same TypeVar for the same label name across multiple params
                            // (same-name label vars must share the same TypeVar).
                            let fresh = if let Some(ref mut mapping) = ann_mapping {
                                if let Some(existing_var) = mapping.get(name.as_str()) {
                                    existing_var.clone()
                                } else {
                                    let n = state.subst.name_counter.get();
                                    let v = format!("_label_{}", n);
                                    state.subst.name_counter.set(n.saturating_add(1));
                                    state.levels.insert(v.clone(), state.level);
                                    state.kind_env.insert(v.clone(), Kind::Label);
                                    state.type_var_source_names.insert(v.clone(), name.clone());
                                    mapping.insert(name.clone(), v.clone());
                                    v
                                }
                            } else {
                                let n = state.subst.name_counter.get();
                                let v = format!("_label_{}", n);
                                state.subst.name_counter.set(n.saturating_add(1));
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
                    _ => Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "label: value must be a bare name (e.g. `label: l`)".to_string(),
                        span,
                        notes: vec![], call_stack: vec![],
                    })),
                }
            } else {
                // No "type:" key (or has non-metadata keys) — treat as structural type or metadata.
                Box::pin(resolve_property_dict_as_record(
                    surface_entries,
                    env,
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    let dict_result = resolve_type_dict(
        entries,
        env,
        span.clone(),
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
    )
    .await;

    match dict_result {
        Ok(ty) => Ok(ty),
        Err(e) => {
            let is_tycon_error = entries.first().is_some_and(|first| {
                first.node.key.is_none()
                    && matches!(&first.node.value.expr, SurfaceExpression::VarRef { name, .. }
                        if env.lookup_tycon_def(name).is_some())
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
                // Return Unknown when type-stage evaluation fails: env unavailable,
                // eval error, or the result is TypeNode.Recursive/RecursiveRef (deferred).
                Ok(eval_type_stage_expr(&synth_node, env, state)
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
        if matches!(&entry.node.value.expr, SurfaceExpression::Rest(..)) {
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
async fn instantiate_type_alias(
    alias: &TypeAlias,
    type_args: &[Type],
    state: &mut InferState,
) -> Result<Type, TypeError> {
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
                    // Clone instance_env to avoid borrow conflict: resolve_instance takes &self
                    // on instance_env AND &mut state simultaneously, which Rust's borrow checker
                    // rejects when both are fields of InferState.
                    let instance_env = state.instance_env.clone();
                    let error_span = origin_span.clone().unwrap_or_else(|| rust_span!());
                    match Box::pin(instance_env.resolve_instance(&class.name, arg_type, state))
                        .await
                    {
                        Ok(Some(_)) => {
                            // Constraint satisfied — continue.
                        }
                        Ok(None) => {
                            let constraint_label = origin_name.as_deref().unwrap_or(&class.name);
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "type argument `{arg_type}` does not satisfy constraint \
                                     `{constraint_label}` — no instance found for class `{}`",
                                    class.name
                                ),
                                span: error_span,
                                notes: vec![],
                                call_stack: vec![],
                            }));
                        }
                        Err(ambiguity_msg) => {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "ambiguous instances for constraint `{}` with type \
                                     argument `{arg_type}`: {ambiguity_msg}",
                                    class.name
                                ),
                                span: error_span,
                                notes: vec![],
                                call_stack: vec![],
                            }));
                        }
                    }
                }
                // If param_name is not in type_subst, it's a bug (params and constraints were
                // built together in register_type_aliases). We silently skip for robustness.
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
            Type::Record(Row {
                fields: new_fields,
                tail: new_tail,
            })
        }
        Type::Function {
            params,
            ret,
            variadic,
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
            ret: Box::new(apply_type_alias_substitution(ret, subst, state)),
            variadic: *variadic,
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
        Type::NominalVariant { tag, fields } => {
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
                tag: tag.clone(),
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
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    // Handle builtin types and lowercase type variables using normal resolution
    if !name.starts_with(|c: char| c.is_uppercase())
        || matches!(
            name,
            "Int"
                | "Float"
                | "String"
                | "Any"
                | "Handle"
                | "Null"
                | "Dict"
                | "Map"
                | "Record"
                | "Fn"
                | "Never"
                | "Unknown"
        )
    {
        let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
        let mut _discard = Vec::new();
        return resolve_type_name(
            name,
            env,
            span,
            state,
            &mut _discard,
            ann_mapping,
            &row_ref,
            type_params_scope,
        );
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
            return Err(TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "type alias '{}' expects {} type parameter(s), got 0",
                    name,
                    alias.params.len()
                ),
                span,
                notes: vec![],
                call_stack: vec![],
            }));
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
    } else if let Some(class_decl) = state.class_env.get(name).cloned() {
        // T-1197: Class name used in annotation position in a recursive/guarded context.
        // Mirrors the same logic in resolve_type_name so both lookup paths handle class
        // names consistently. See resolve_type_name for the full rationale.
        let n = state.subst.name_counter.get();
        let fresh = format!("_t{}", n);
        state.subst.name_counter.set(n.saturating_add(1));
        state.levels.insert(fresh.clone(), state.level);
        // Construct the Constraint::Class that @C should produce in this guarded path.
        // resolve_type_name_with_guard does not carry a constraints parameter (unlike
        // resolve_type_name), so we cannot push it to the caller's constraint vec. A local
        // vec records the constraint to make the intent explicit. The correct fix is to add
        // a `constraints: &mut Vec<Constraint>` parameter to this function so the constraint
        // reaches the caller's SCC constraint collection — tracked as part of T-1206 scope.
        // Without that threading, @C in a recursive/guarded type annotation context produces
        // a TypeVar that is unconstrained at the unification site: a known limitation.
        let mut _local_constraints: Vec<Constraint> = Vec::new();
        _local_constraints.push(Constraint::Class {
            class: Arc::new(class_decl),
            vars: vec![ConstraintArg::Var(fresh.clone())],
            origin_name: None,
            origin_span: Some(span),
        });
        // TODO(T-1206): thread _local_constraints into caller via constraints parameter
        Ok(Type::TypeVar(fresh, state.level))
    } else {
        Err(TypeErrorTyped::UndefinedType(UndefinedType {
            name: name.to_string(),
            span,
            notes: vec![],
            call_stack: vec![],
        }))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_type_name(
    name: &str,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &Option<&HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    match name {
        "Int" => Ok(Type::Int),
        "Float" => Ok(Type::Float),
        "String" | "Str" => Ok(Type::Str),
        "Bytes" => Ok(Type::Bytes),
        "Any" => Ok(Type::Any),
        "Proxy" => Ok(Type::Proxy),
        // BAS type names
        "Never" => Ok(Type::Never),
        "Unknown" => Ok(Type::Unknown),
        "Operator" => Err(TypeErrorTyped::Generic(GenericTypeError {
            message: "Operator is a kind, not a type — annotate a class type parameter as `f@Operator`, not a value expression".to_string(),
            span,
            notes: vec![], call_stack: vec![],
        })),
        "Label" => {
            // Anonymous Label-kinded TypeVar (parallel to `@Operator` error above).
            // Create a fresh system-generated name like `_label_0`.
            // This is for when the label TypeVar is not referenced elsewhere (e.g., `get`/`get-or`).
            let n = state.subst.name_counter.get();
            let fresh = format!("_label_{}", n);
            state.subst.name_counter.set(n.saturating_add(1));
            state.levels.insert(fresh.clone(), state.level);
            state.kind_env.insert(fresh.clone(), crate::types::Kind::Label);
            Ok(Type::TypeVar(fresh, state.level))
        }
        // Bare @Handle — no capability row argument. Resolves to Handle(Unknown),
        // which is the gradual "any handle" type. This is correct for unannotated
        // handle parameters where the caller doesn't know (or care about) the
        // capability row. Parameterized forms (`h@Handle@DirCap`, `[Handle DirCap]`,
        // `@Handle@DirCap`) resolve through resolve_annotated/resolve_annotation/
        // resolve_type_dict respectively and never reach this bare-name path.
        "Handle" => Ok(Type::handle(Type::Unknown)),
        "Null" => Ok(Type::Record(Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        })),
        "Dict" => {
            // Empty record — represents "any dict" under BAS width subtyping.
            // Any concrete record is a subtype because all required fields (none) are present.
            Ok(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }))
        }
        "Map" => {
            // Bare @Map → Map[Unknown: Unknown]
            Ok(Type::map(Type::Unknown, Type::Unknown))
        }
        "Record" => {
            // Bare @Record → open record (empty fields)
            Ok(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
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
                ret: Box::new(Type::Any),
                variadic: true,
                required_count: 0,
            })
        }
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
                    // allow extra TypeVars to fall through to ann_mapping / fresh_type_var.
                    let in_params = ann_mapping.as_ref().is_some_and(|m| m.contains_key(name));
                    if strict && !in_params && !params.contains_key(name) {
                        // Name not declared as a type parameter — check if it's a scope reference.
                        if env.get_type_alias(name).is_none() && env.lookup_tycon_def(name).is_none() {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "undefined name '{name}' in type alias body — \
                                     lowercase names must be declared as type parameters \
                                     with [let ...] or must refer to a type in scope"
                                ),
                                span,
                                notes: vec![], call_stack: vec![],
                            }));
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
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "annotation name '{name}' is already used as a row variable in this function; \
                             it cannot also be used as a type variable"
                        ),
                        span,
                        notes: vec![], call_stack: vec![],
                    }));
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
                        let n = state.subst.name_counter.get();
                        let fresh = format!("_t{}", n);
                        state.subst.name_counter.set(n.saturating_add(1));
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
                    // Nominal ADT check must happen before the arity check so that parameterized
                    // nominal ADTs (e.g. Result, Seq, Maybe) can be referenced bare as type
                    // constructors in HKT-style instance arm annotations ([let f@Result]).
                    // Returns TyCon(name) which UNIFY-TYCON-EXPAND can match against variants.
                    if let Some(def) = env.lookup_tycon_def(name) {
                        if !def.constructors.is_empty() || def.builtin_type.is_some() {
                            return Ok(Type::TyCon(name.to_string()));
                        }
                    }

                    // Check arity — bare alias name must have zero parameters for non-ADT aliases.
                    if !alias.params.is_empty() {
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "type alias '{}' expects {} type parameter(s), got 0",
                                name,
                                alias.params.len()
                            ),
                            span,
                            notes: vec![], call_stack: vec![],
                        }));
                    }

                    // Non-nominal zero-parameter alias: return the body directly.
                    // Recursive expansion happens during alias registration via resolve_type_name_with_guard.
                    Ok(alias.body.clone())
                } else if let Some(class_decl) = state.class_env.get(name).cloned() {
                    // T-1197: Class name used in annotation position: @Comparable, @Equatable, etc.
                    //
                    // When a name in annotation position resolves to a ClassDecl (not a type alias),
                    // introduce a fresh type variable constrained by that class. The class constraint
                    // is added to the threaded `constraints` parameter so that when this TypeVar is later unified with a
                    // concrete type at a call site, check_constraints_on_var fires and verifies the
                    // instance exists (T-1198).
                    //
                    // Each occurrence of @C creates an independent fresh TypeVar — class names are
                    // not in the type variable namespace (unlike lowercase annotation names such as
                    // @a which are deduplicated via ann_mapping). Two parameters x@Comparable and
                    // y@Comparable get distinct TypeVars _tN and _tM, each independently constrained.
                    let n = state.subst.name_counter.get();
                    let fresh = format!("_t{}", n);
                    state.subst.name_counter.set(n.saturating_add(1));
                    state.levels.insert(fresh.clone(), state.level);
                    constraints.push(Constraint::Class {
                        class: Arc::new(class_decl),
                        vars: vec![ConstraintArg::Var(fresh.clone())],
                        origin_name: None,
                        origin_span: Some(span),
                    });
                    Ok(Type::TypeVar(fresh, state.level))
                } else {
                    Err(TypeErrorTyped::UndefinedType(UndefinedType {
                        name: name.to_string(),
                        span,
                        notes: vec![], call_stack: vec![],
                    }))
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
        return Err(TypeErrorTyped::Generic(GenericTypeError {
            message: format!(
                "recursive type alias '{}' exceeds maximum unfolding depth ({})",
                current_alias, MAX_ALIAS_DEPTH
            ),
            span,
            notes: vec![],
            call_stack: vec![],
        }));
    }

    // Add current alias to guard
    alias_guard.insert(current_alias.to_string());

    // Recursively expand the type structure
    let result = match ty {
        Type::Record(row) => {
            let mut new_fields = indexmap::IndexMap::new();
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
            Ok(Type::Record(Row {
                fields: new_fields,
                tail: crate::type_def::RowTail::Empty,
            }))
        }
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => {
            let new_params = params
                .iter()
                .map(|(name, p_ty)| {
                    Ok::<_, TypeError>((
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
                required_count: *required_count,
            })
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
        Type::App(f, arg) => {
            let new_f = expand_alias_body_guarded(
                f,
                env,
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
                env,
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
    env: &TypeEnv,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    const MAX_ALIAS_DEPTH: usize = 256;
    if depth >= MAX_ALIAS_DEPTH {
        return Err(TypeErrorTyped::Generic(GenericTypeError {
            message: format!(
                "recursive type alias '{}' exceeds maximum unfolding depth ({})",
                current_alias, MAX_ALIAS_DEPTH
            ),
            span: node.span.clone(),
            notes: vec![],
            call_stack: vec![],
        }));
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
            type_params_scope,
        ),
        SurfaceExpression::Dict(entries) => {
            Box::pin(resolve_type_dict_with_guard(
                entries,
                env,
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
                env,
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    recursion_guard: &mut HashSet<String>,
    current_alias: &str,
    depth: usize,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
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
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[or ...] requires at least one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = Box::pin(resolve_type_expr_with_guard(
                        &entry.node.value,
                        env,
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
                    members.push(ty);
                }
                return Ok(Type::normalize_union(members));
            } else if kw == "all" {
                // [all T1 T2 ...] → Intersection([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[all ...] requires at least one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = Box::pin(resolve_type_expr_with_guard(
                        &entry.node.value,
                        env,
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
                    members.push(ty);
                }
                return Ok(Type::normalize_intersection(members));
            } else if kw == "without" {
                // [without A] → Negation(A)
                if entries.len() != 2 {
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[without A] requires exactly one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let inner = Box::pin(resolve_type_expr_with_guard(
                    &entries[1].node.value,
                    env,
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
    // Positional-only forms (Fn types, [Seq T], [Map K V], parameterized aliases,
    // unions) are not keyed, so they fall through to `resolve_type_dict` as before.
    if !all_positional {
        // Reuse the same logic as the tail of resolve_type_dict but route field-value
        // resolution through resolve_type_expr_with_guard.
        let mut fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
        for entry in entries {
            if let SurfaceExpression::Rest(..) = &entry.node.value.expr {
                // `...` rest notation: accepted for openness annotation, produces no field.
                continue;
            }
            let key = match &entry.node.key {
                Some(k) => match &k.expr {
                    SurfaceExpression::Str(s) => s.clone(),
                    // Field with annotation: `field@Child: Type` (T-1052).
                    // The annotation is stored in the SurfaceNode for T-1053 to process;
                    // type resolution uses only the field name.
                    SurfaceExpression::Annotated { name, .. } => name.clone(),
                    _ => {
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: "type record keys must be bare words".to_string(),
                            span: k.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }))
                    }
                },
                None => {
                    // Mixed keyed+positional dict — fall back to the full resolver.
                    return resolve_type_dict(
                        entries,
                        env,
                        span,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await;
                }
            };
            let ty = Box::pin(resolve_type_expr_with_guard(
                &entry.node.value,
                env,
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
                        Type::Record(Row {
                            fields: member_fields,
                            tail: crate::type_def::RowTail::Empty,
                        })
                    })
                    .collect();
                return Ok(Type::normalize_intersection(members));
            }
        }

        let ty = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });
        crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
        return Ok(ty);
    }

    // For remaining positional-only cases (function types, [Seq T], [Map K V],
    // parameterized alias applications, and multi-type unions), delegate to the normal
    // resolver which has the full dispatch logic for those forms.
    resolve_type_dict(
        entries,
        env,
        span,
        state,
        constraints,
        ann_mapping,
        row_ann_mapping,
        type_params_scope,
    )
    .await
}

pub(crate) async fn resolve_type_expr(
    node: &Arc<SurfaceNode>,
    env: &TypeEnv,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
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
            match resolve_type_name(
                name,
                env,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                &row_ref,
                type_params_scope,
            ) {
                Ok(ty) => Ok(ty),
                Err(e) if crate::eval::is_constructor_name(name) => {
                    // Unknown uppercase name: treat as a zero-payload nominal variant constructor.
                    // This handles variant tags in type alias bodies such as `None` in
                    // `[type [Option a] [Some a] None]` where `None` has no payload.
                    let _ = e; // suppress the undefined-type error
                    Ok(Type::NominalVariant {
                        tag: name.clone(),
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
                env,
                node.span.clone(),
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            ))
            .await
        }
        SurfaceExpression::Annotated { name, annotation } => {
            if name == "Fn" {
                Box::pin(resolve_fn_type(
                    &annotation.node,
                    env,
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
                //
                // Previously this called `resolve_annotation(&annotation.node, ...)` which
                // dropped `name` entirely and resolved only the inner annotation — losing
                // the Handle wrapper for `Handle@DirCap`, Seq wrapper for `Seq@Int`, etc.
                let full_ann =
                    Annotation::Annotated(name.clone(), Box::new(annotation.node.clone()));
                Box::pin(resolve_annotation(
                    &full_ann,
                    env,
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
                    let ret = Box::pin(resolve_annotation_as_type(
                        &annotation.node,
                        env,
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
                                            SurfaceExpression::Str(s) => Some(s.clone()),
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    };
                                    let param_ty = Box::pin(resolve_type_expr(
                                        &entry.node.value,
                                        env,
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
                                    env,
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
                                        env,
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
                                    env,
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
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                                1 + args.len()
                            ),
                            span: node.span.clone(),
                            notes: vec![], call_stack: vec![],
                        }));
                    }
                    let required_count = params.len();
                    return Ok(Type::Function {
                        params,
                        ret: Box::new(ret),
                        variadic: false,
                        required_count,
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
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: "[or ...] requires at least one type argument".to_string(),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
                    }
                    let mut members = Vec::new();
                    for arg in args.iter() {
                        let ty = Box::pin(resolve_type_expr(
                            arg,
                            env,
                            state,
                            constraints,
                            ann_mapping,
                            row_ann_mapping,
                            type_params_scope,
                        ))
                        .await?;
                        members.push(ty);
                    }
                    return Ok(Type::normalize_union(members));
                } else if kw == "all" {
                    // args contains the type arguments; func ("all") is the head, not a type.
                    if args.is_empty() {
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: "[all ...] requires at least one type argument".to_string(),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
                    }
                    let mut members = Vec::new();
                    for arg in args.iter() {
                        let ty = Box::pin(resolve_type_expr(
                            arg,
                            env,
                            state,
                            constraints,
                            ann_mapping,
                            row_ann_mapping,
                            type_params_scope,
                        ))
                        .await?;
                        members.push(ty);
                    }
                    return Ok(Type::normalize_intersection(members));
                } else if kw == "without" {
                    if args.len() != 1 {
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: "[without A] requires exactly one type argument".to_string(),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
                    }
                    let inner = Box::pin(resolve_type_expr(
                        &args[0],
                        env,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    ))
                    .await?;
                    return Ok(Type::Negation(Box::new(inner)));
                }
            }

            // TyConDef-based type constructor application (T-949) in implied-call position.
            // Primary path for user-defined type constructors in [TyCon Arg1 Arg2 ...] form.
            // Primary path: look up via TyConDef (covers user-defined types and builtin TyCons
            // registered in T-1018: Seq, Map, Handle). Falls through to kind_env for unregistered names.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if let Some(def) = env.lookup_tycon_def(name) {
                    let arity = def.arity();
                    if arity > 0 {
                        let mut result = Type::TyCon(name.clone());
                        let arg_count = std::cmp::min(arity, args.len());
                        for arg_node in args.iter().take(arg_count) {
                            let arg = Box::pin(resolve_type_expr(
                                arg_node,
                                env,
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
                if let Some(alias) = env.get_type_alias(name) {
                    // Resolve all type arguments
                    let mut type_args = Vec::new();
                    for arg in args {
                        type_args.push(
                            Box::pin(resolve_type_expr(
                                arg,
                                env,
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
                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "type alias '{}' expects {} type parameter(s), got {}",
                                name,
                                alias.params.len(),
                                type_args.len()
                            ),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
                    }

                    // Build substitution and apply to body
                    return Box::pin(instantiate_type_alias(&alias, &type_args, state)).await;
                }
            }

            // Nominal constructor: [ConstructorName field1: T1 field2: T2 ...]
            // Check if func is an uppercase VarRef (nominal constructor name).
            // Builtin type names (Int, Float, etc.) must NOT be treated as NominalVariant.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                let is_builtin = state
                    .tycon_env
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
                    // `is_literal` helper: true for Int, U64, Float, Str surface expressions.
                    let is_literal_expr = |expr: &SurfaceExpression| {
                        matches!(
                            expr,
                            SurfaceExpression::Int(_)
                                | SurfaceExpression::U64(_)
                                | SurfaceExpression::Float(_)
                                | SurfaceExpression::Str(_)
                        )
                    };

                    // Collect payload fields from non-literal named_args.
                    let payload_named: Vec<_> = named_args
                        .iter()
                        .filter(|na| !is_literal_expr(&na.node.value.expr))
                        .collect();

                    // Collect payload fields from annotated positional args (data@String form).
                    let payload_annotated: Vec<_> = args
                        .iter()
                        .filter_map(|arg| {
                            if let SurfaceExpression::Annotated { name, annotation } = &arg.expr {
                                Some((name.clone(), annotation.clone(), arg.span.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Collect non-annotated positional args (old-style [Some a] payload).
                    let positional_non_annotated: Vec<_> = args
                        .iter()
                        .filter(|arg| !matches!(&arg.expr, SurfaceExpression::Annotated { .. }))
                        .collect();

                    let has_payload_named = !payload_named.is_empty();
                    let has_payload_annotated = !payload_annotated.is_empty();
                    let has_positional = !positional_non_annotated.is_empty();

                    if has_payload_named || has_payload_annotated {
                        // Mixed constants + payload fields, or payload-only.
                        // Build NominalVariant with only the payload fields (constants live in TyConDef).
                        let mut fields_map = indexmap::IndexMap::new();

                        // Named payload fields from non-literal named_args.
                        for named_arg in &payload_named {
                            let field_ty = Box::pin(resolve_type_expr(
                                &named_arg.node.value,
                                env,
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
                                env,
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
                            tag: name.clone(),
                            fields: Row {
                                fields: fields_map,
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
                    } else if has_positional {
                        // Old-style single positional payload: [Some a] → NominalVariant("Some", { "0": a })
                        // Only valid when exactly one non-annotated positional arg is present.
                        if positional_non_annotated.len() == 1 {
                            let payload_ty = Box::pin(resolve_type_expr(
                                positional_non_annotated[0],
                                env,
                                state,
                                constraints,
                                ann_mapping,
                                row_ann_mapping,
                                type_params_scope,
                            ))
                            .await?;
                            let mut fields_map = indexmap::IndexMap::new();
                            fields_map.insert("0".to_string(), payload_ty);
                            return Ok(Type::NominalVariant {
                                tag: name.clone(),
                                fields: Row {
                                    fields: fields_map,
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            });
                        } else {
                            return Err(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "nominal constructor {} requires either 0 args, 1 positional arg, or named/annotated payload fields",
                                    name
                                ),
                                span: node.span.clone(),
                                notes: vec![], call_stack: vec![],
                            }));
                        }
                    } else {
                        // No payload: all named_args are constants, no annotated positionals.
                        // Unit constructor (with or without constants): NominalVariant with empty fields.
                        return Ok(Type::NominalVariant {
                            tag: name.clone(),
                            fields: Row {
                                fields: indexmap::IndexMap::new(),
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
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
                        constraints,
                        ann_mapping,
                        &row_ann_mapping.as_ref().map(|m| &**m),
                        type_params_scope,
                    )?;
                    let mut members = vec![head_ty];
                    for arg in args.iter() {
                        let member_ty = Box::pin(resolve_type_expr(
                            arg,
                            env,
                            state,
                            constraints,
                            ann_mapping,
                            row_ann_mapping,
                            type_params_scope,
                        ))
                        .await?;
                        members.push(member_ty);
                    }
                    return Ok(Type::normalize_union(members));
                }
            }

            Err(TypeErrorTyped::Generic(GenericTypeError {
                message: format!("invalid type expression in annotation: {:?}", node.expr),
                span: node.span.clone(),
                notes: vec![],
                call_stack: vec![],
            }))
        }
        _ => Err(TypeErrorTyped::Generic(GenericTypeError {
            message: format!("invalid type expression in annotation: {:?}", node.expr),
            span: node.span.clone(),
            notes: vec![],
            call_stack: vec![],
        })),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_type_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
) -> Result<Type, TypeError> {
    if let Some(fn_type) = Box::pin(try_resolve_fn_type_expr(
        entries,
        env,
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

    // TyConDef-based type constructor application (T-949).
    // Primary path for user-defined and builtin type constructors declared in TyConEnv.
    //
    // Structural aliases (no constructors, no builtin_type) are expanded via expand_named
    // (Phase 2 of parameterized-type-aliases whatif): [Pair Int] with
    // Pair = [type [let a] [first: a second: a]] → Record({first: Int, second: Int}).
    //
    // ADTs and builtins produce left-associative App chains: [Tree Int] → App(TyCon("Tree"), Int).
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    if let Some(def) = env.lookup_tycon_def(name) {
                        let arity = def.arity();
                        if arity == 0 && entries.len() == 1 {
                            // Zero-arity TyCon: expand via expand_named, which handles
                            // structural aliases and returns Type::TyCon for builtins/ADTs.
                            return Ok(expand_named(name, &[], env, state)
                                .unwrap_or_else(|| Type::TyCon(name.clone())));
                        } else if arity > 0 {
                            // Collect argument types from subsequent positional entries.
                            if entries.len() < 1 + arity {
                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                    message: format!(
                                        "type constructor '{}' requires {} argument(s), got {}",
                                        name,
                                        arity,
                                        entries.len() - 1
                                    ),
                                    span,
                                    notes: vec![],
                                    call_stack: vec![],
                                }));
                            }
                            let mut args = Vec::with_capacity(arity);
                            for entry in entries.iter().take(arity + 1).skip(1) {
                                let arg = resolve_type_expr(
                                    &entry.node.value,
                                    env,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                )
                                .await?;
                                args.push(arg);
                            }

                            // Expand via expand_named (handles structural aliases, builtins,
                            // and ADTs uniformly). Falls back to App(TyCon, args) chain when
                            // expand_named returns None (unknown alias name).
                            return Ok(expand_named(name, &args, env, state).unwrap_or_else(
                                || {
                                    let mut result = Type::TyCon(name.clone());
                                    for arg in &args {
                                        result = Type::App(Box::new(result), Box::new(arg.clone()));
                                    }
                                    result
                                },
                            ));
                        }
                    }
                }
            }
        }
    }

    // General type constructor application via kind_env — the permanent dispatch path for
    // user-defined Operator-kinded class params (e.g., `m` in `[class [m@Operator] ...]`).
    // Seq/Map/Handle are registered in TyConDef (T-1018) and caught by the TyConDef path above;
    // they never reach this code path. Must run BEFORE the parameterized alias lookup so
    // Operator-kinded class params take priority over parameterized type aliases.
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    if let Some(kind) = state.kind_env.get(name.as_str()) {
                        if kind.arity() > 0 {
                            let mut args: Vec<Type> = Vec::new();
                            for e in entries[1..].iter() {
                                args.push(
                                    resolve_type_expr(
                                        &e.node.value,
                                        env,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    )
                                    .await?,
                                );
                            }

                            // User-defined: always Kind::Operator (arity 1).
                            // Rank-1 restriction: argument cannot itself be a type constructor.
                            // Note: Seq/Map/Handle are caught by the TyConDef path above and
                            // never reach here (T-1021/T-1018).
                            if args.len() != 1 {
                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                    message: format!(
                                        "type constructor `{name}` requires 1 type argument, got {}",
                                        args.len()
                                    ),
                                    span,
                                    notes: vec![], call_stack: vec![],
                                }));
                            }
                            let a_type = args.into_iter().next().unwrap();
                            if let Type::Operator(op_name) = &a_type {
                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                    message: format!(
                                        "kind mismatch: type constructor `{name}` cannot be \
                                         applied to another type constructor `{op_name}`; \
                                         use a concrete type instead"
                                    ),
                                    span,
                                    notes: vec![],
                                    call_stack: vec![],
                                }));
                            }
                            return Ok(Type::App(
                                Box::new(Type::Operator(name.clone())),
                                Box::new(a_type),
                            ));
                        }
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
                                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                                        message: format!(
                                            "unexpected keyed entry in type alias application '{}'",
                                            name
                                        ),
                                        span: entry.span.clone(),
                                        notes: vec![],
                                        call_stack: vec![],
                                    }));
                                }
                                type_args.push(
                                    resolve_type_expr(
                                        &entry.node.value,
                                        env,
                                        state,
                                        constraints,
                                        ann_mapping,
                                        row_ann_mapping,
                                        type_params_scope,
                                    )
                                    .await?,
                                );
                            }
                            if type_args.len() != alias.params.len() {
                                return Err(TypeErrorTyped::Generic(GenericTypeError {
                                    message: format!(
                                        "type alias '{}' expects {} type parameter(s), got {}",
                                        name,
                                        alias.params.len(),
                                        type_args.len()
                                    ),
                                    span,
                                    notes: vec![],
                                    call_stack: vec![],
                                }));
                            }
                            return Box::pin(instantiate_type_alias(&alias, &type_args, state))
                                .await;
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
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[or ...] requires at least one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr(
                        &entry.node.value,
                        env,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await?;
                    members.push(ty);
                }
                return Ok(Type::normalize_union(members));
            } else if kw == "all" {
                // [all T1 T2 ...] → Intersection([T1, T2, ...])
                if entries.len() < 2 {
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[all ...] requires at least one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let mut members = Vec::new();
                for entry in &entries[1..] {
                    let ty = resolve_type_expr(
                        &entry.node.value,
                        env,
                        state,
                        constraints,
                        ann_mapping,
                        row_ann_mapping,
                        type_params_scope,
                    )
                    .await?;
                    members.push(ty);
                }
                return Ok(Type::normalize_intersection(members));
            } else if kw == "without" {
                // [without A] → Negation(A)
                if entries.len() != 2 {
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "[without A] requires exactly one type argument".to_string(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
                let inner = resolve_type_expr(
                    &entries[1].node.value,
                    env,
                    state,
                    constraints,
                    ann_mapping,
                    row_ann_mapping,
                    type_params_scope,
                )
                .await?;
                return Ok(Type::Negation(Box::new(inner)));
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
                // Extract the constructor tag name from VarRef or Annotated (T-1052).
                // `[Constructor field: Type ...]` — first entry is VarRef.
                // `[Constructor@[as-type: fn  guarding: false] field: Type ...]` — first entry
                // is Annotated { name: "Constructor", annotation: PropertyDict([...]) }.
                // The annotation carries type-level metadata; the name is the constructor tag.
                // Both forms resolve identically for type-checking purposes; T-1053 reads
                // the annotation from the SurfaceNode tree to populate FnAnnotation.extra.
                let tag_opt: Option<String> = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                    SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                    _ => None,
                };
                if let Some(tag) = tag_opt {
                    // `tag` is an owned String; pass as &str where needed.
                    // Check if tag is uppercase (constructor name).
                    // BUT: builtin type names (Int, Float, String, Bool, Number, etc.) also
                    // start with uppercase and must NOT be treated as NominalVariant.
                    // Resolve builtin type names through resolve_type_name first.
                    let is_builtin_type = state
                        .tycon_env
                        .get(&tag)
                        .is_some_and(|def| def.builtin_type.is_some());
                    if is_builtin_type && entries.len() == 1 && first.node.key.is_none() {
                        // Single positional entry that is a builtin type name: [Int] → Type::Int.
                        // This handles annotations like @[Int] which should resolve to Int,
                        // not to NominalVariant { tag: "Int" }.
                        let row_ref: Option<&HashMap<String, String>> =
                            row_ann_mapping.as_ref().map(|m| &**m);
                        return resolve_type_name(
                            &tag,
                            env,
                            span,
                            state,
                            constraints,
                            ann_mapping,
                            &row_ref,
                            type_params_scope,
                        );
                    }
                    if crate::eval::is_constructor_name(&tag) && !is_builtin_type {
                        // Case 1: Pure positional — [Constructor] or [Constructor PayloadType]
                        let all_remaining_positional =
                            entries[1..].iter().all(|e| e.node.key.is_none());
                        if all_remaining_positional {
                            if entries.len() == 1 {
                                // Unit constructor: [None]
                                return Ok(Type::NominalVariant {
                                    tag: tag.to_string(),
                                    fields: Row {
                                        fields: indexmap::IndexMap::new(),
                                        tail: crate::type_def::RowTail::Empty,
                                    },
                                });
                            } else if entries.len() == 2 {
                                // Single-payload constructor: [Ok a]
                                let payload_ty = resolve_type_expr(
                                    &entries[1].node.value,
                                    env,
                                    state,
                                    constraints,
                                    ann_mapping,
                                    row_ann_mapping,
                                    type_params_scope,
                                )
                                .await?;
                                // Unnamed payload: create record with single field "0"
                                let mut fields = indexmap::IndexMap::new();
                                fields.insert("0".to_string(), payload_ty);
                                return Ok(Type::NominalVariant {
                                    tag: tag.to_string(),
                                    fields: Row {
                                        fields,
                                        tail: crate::type_def::RowTail::Empty,
                                    },
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
                                            SurfaceExpression::Str(s) => s.clone(),
                                            SurfaceExpression::Annotated { name, .. } => {
                                                name.clone()
                                            }
                                            _ => return Err(TypeErrorTyped::Generic(GenericTypeError {
                                                message: "nominal variant field names must be bare words".to_string(),
                                                span: k.span.clone(),
                                                notes: vec![], call_stack: vec![],
                                            })),
                                        };
                                        let field_ty = resolve_type_expr(
                                            &field_entry.node.value,
                                            env,
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
                                        return Err(TypeErrorTyped::Generic(GenericTypeError {
                                            message: "nominal variant constructor with named fields requires all fields after the constructor tag to be keyed (field: Type)".to_string(),
                                            span: field_entry.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        }));
                                    }
                                }
                            }
                            return Ok(Type::NominalVariant {
                                tag: tag.to_string(),
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
        if let SurfaceExpression::Rest(_name, _) = &entry.node.value.expr {
            // BAS: `...` annotations express user intent for openness; under BAS width
            // subtyping all records are closed — is_subtype handles extra fields.
            has_rest = true;
            continue;
        }

        // Column constraint sentinel: key is `_` (bare wildcard) or `_@K` (typed wildcard).
        // Recognized in key position: SurfaceExpression::VarRef { name: "_" } or
        // SurfaceExpression::Annotated { name: "_", annotation: K }.
        let is_wildcard_key = match &entry.node.key {
            Some(k) => match &k.expr {
                SurfaceExpression::VarRef { name, .. } if name == "_" => true,
                SurfaceExpression::Annotated { name, .. } if name == "_" => true,
                _ => false,
            },
            None => false,
        };

        if is_wildcard_key {
            if uniform_tail.is_some() {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: "duplicate uniform-field sentinel `_` in row type annotation — at most one `_` allowed per row".to_string(),
                    span: entry.span.clone(),
                    notes: vec![], call_stack: vec![],
                }));
            }
            let value_ty = resolve_type_expr(
                &entry.node.value,
                env,
                state,
                constraints,
                ann_mapping,
                row_ann_mapping,
                type_params_scope,
            )
            .await?;
            // Check for typed-key form `_@K` vs plain `_`
            let key_ty = match entry.node.key.as_ref().map(|k| &k.expr) {
                Some(SurfaceExpression::Annotated { annotation, .. }) => {
                    // `_@K`: resolve K as the key type constraint.
                    let key_t = resolve_annotation(
                        &annotation.node,
                        env,
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
                SurfaceExpression::Str(s) => s.clone(),
                // Annotated field key: `field@Child: Type` (T-1052).
                // The annotation is metadata for T-1053; type resolution uses only the name.
                SurfaceExpression::Annotated { name, .. } => name.clone(),
                _ => {
                    return Err(TypeErrorTyped::Generic(GenericTypeError {
                        message: "type record keys must be bare words".to_string(),
                        span: k.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }))
                }
            },
            None => {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: "auto-indexed entries not supported in type expressions".to_string(),
                    span: entry.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                }))
            }
        };
        let ty = resolve_type_expr(
            &entry.node.value,
            env,
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
                    Type::Record(Row {
                        fields: member_fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                })
                .collect();
            return Ok(Type::normalize_intersection(members));
        }
    }

    let ty = Type::Record(Row {
        fields,
        tail: effective_tail,
    });
    crate::types::check_kind_wellformed(&ty, &state.kind_env, span)?;
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
/// conversion at `parse_annotation` line ~565: `SurfaceExpression::Call { implied: true }`
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
    // is a VarRef. This matches the parser rule at parse_annotation.
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
        } => *id,
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
            for (key, thunk_id) in &dict {
                if let HashableValue::Str(k) = key {
                    let field_thunk = ctx.get_thunk(*thunk_id);
                    if let Ok(v) = crate::eval::materialize(&field_thunk, None, ctx).await {
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
    // Unwrap Value::Annotated wrapper transparently.
    let inner = match dict_val {
        Value::Annotated { inner, .. } => inner.as_ref().clone(),
        other => other,
    };

    let dict = match inner {
        Value::Dict(d) => d,
        _ => return None,
    };

    let mut result = Vec::new();
    let mut i = 0i64;
    loop {
        match dict.get(&HashableValue::Int(i)) {
            Some(thunk_id) => {
                let thunk = ctx.get_thunk(*thunk_id);
                let val = crate::eval::materialize(&thunk, None, ctx).await.ok()?;
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
/// Public (crate-internal) re-export for use by `type_normalize::evaluate_resolver`.
/// The implementation is `typenode_value_to_type`; this wrapper adds the `pub(crate)` visibility.
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
            // Annotated wrapper — TypeNode constructors with @[...] annotations on their
            // constructor names (e.g. `[Int@[as-type: [fn [let t] t]  guarding: true]]`) are
            // wrapped in Value::Annotated at runtime. The inner value is the bare Variant;
            // the annotation dict carries the constructor metadata. Unwrap transparently.
            Value::Annotated { inner, .. } => typenode_value_to_type(inner, ctx).await,

            // TypeNode Variant values produced by the TypeNode ADT (T-1058 / T-1061).
            Value::Variant { tag, payload: _ } => {
                match tag.as_str() {
                    // ── Primitive leaf constructors ──────────────────────────────────────
                    // No payload — map directly to concrete Type variants.
                    "TypeNode.Int" => Some(Type::Int),
                    "TypeNode.Float" => Some(Type::Float),
                    "TypeNode.String" => Some(Type::Str),
                    "TypeNode.Bool" => Some(Type::TyCon("Boolean".to_string())),
                    "TypeNode.Unknown" => Some(Type::Unknown),
                    // TypeNode.Top is the sound lattice top (τ <: Top for all τ).
                    // Rust represents this as Type::Any (which IS the top type in the lattice).
                    // Distinct from TypeNode.Unknown (the gradual ? type, not in the subtype lattice).
                    "TypeNode.Top" => Some(Type::Any),
                    "TypeNode.Never" => Some(Type::Never),
                    "TypeNode.Absent" => Some(Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    })),

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

                    // ── Record ───────────────────────────────────────────────────────────
                    // TypeNode.Record { fields: [Map String TypeNode], open: Bool }
                    // → Type::Record(Row { fields: BTreeMap<String, Type>, tail: Empty | Uniform })
                    "TypeNode.Record" => {
                        let payload_fields = variant_payload_dict(val, ctx).await?;
                        let fields_val = payload_fields.get("fields")?.clone();
                        let open_val = payload_fields.get("open")?.clone();

                        // `fields` is a Dict (Map String TypeNode) — string-keyed, values are TypeNodes.
                        let record_fields = match fields_val {
                            Value::Dict(ref dict) => {
                                let mut out: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
                                for (key, thunk_id) in dict {
                                    if let HashableValue::Str(k) = key {
                                        let thunk = ctx.get_thunk(*thunk_id);
                                        let v = crate::eval::materialize(&thunk, None, ctx)
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

                        let tail = if open_val.as_bool() == Some(true) {
                            crate::type_def::RowTail::Uniform {
                                key: None,
                                value: Box::new(Type::Unknown),
                            }
                        } else {
                            crate::type_def::RowTail::Empty
                        };

                        Some(Type::Record(Row {
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
                            variadic: false,
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
                            "Int" => Some(Type::Int),
                            "Float" => Some(Type::Float),
                            "String" | "Str" => Some(Type::Str),
                            "Unknown" => Some(Type::Unknown),
                            "Never" => Some(Type::Never),
                            "Absent" => Some(Type::Record(Row {
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

            // Not a Dict, Annotated, or Variant — cannot be a TypeNode value.
            _ => None,
        }
    })
}

/// Evaluate a type-stage tinct function value with the given arguments and convert the
/// result to a `Type`.
///
/// This is the inner call protocol for type-stage dispatch — used by
/// `eval_type_stage_expr` to invoke `TypeNode.as-type` (or any other type-stage
/// function value) with pre-materialized argument values.
///
/// ## Behaviour
///
/// 1. Allocates materialized thunks for each argument value in `args`.
/// 2. Calls `fn_val` synchronously via `invoke_function_sync` using a minimal
///    `EvalContext` backed by the type-stage environment.
/// 3. Materializes the result thunk.
/// 4. Converts the result via `typenode_value_to_type`.
///
/// Returns `Err(TypeError)` if:
/// - `fn_val` is not a function value.
/// - The type-stage environment is unavailable (`state.type_stage_env` is `None`).
/// - Function invocation or materialization fails.
/// - The result cannot be converted to a `Type` (unrecognized TypeNode tag).
///
/// ## Usage
///
/// Called from `eval_type_stage_expr` to apply `TypeNode.as-type` normalization after
/// evaluating an annotation expression. Also callable directly when the function value is
/// already in hand (e.g., an `as-type:` fn extracted from a constructor annotation).
///
/// `state` contains the user file's extended type-stage env (T-1175) when available. Used
/// to provide the correct evaluation environment for type-stage function calls.
pub(crate) async fn eval_type_stage_value(
    fn_val: &Value,
    args: &[Value],
    state: &mut InferState,
) -> Result<Type, TypeError> {
    let origin_span = rust_span!();

    // Obtain the type-stage environment for building the EvalContext.
    // Prefer state.type_stage_env (set when the source file has --- stage: type sections).
    // Fall back to the prelude type-stage env when state.type_stage_env is None (e.g., for
    // files that use built-in type annotations but declare no type-stage sections of their own).
    let type_stage_env = match state.type_stage_env.clone() {
        Some(env) => env,
        None => match crate::imports::get_prelude_type_stage_env().await {
            Some(env) => env,
            None => return Err(TypeErrorTyped::Generic(GenericTypeError {
                message:
                    "type-stage environment unavailable: prelude type-stage env could not be built"
                        .to_string(),
                span: origin_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })),
        },
    };

    // Build a minimal EvalContext backed by the type-stage environment.
    // AMBIENT-OK: type-stage evaluation performs no file I/O.
    #[allow(clippy::disallowed_methods)]
    let base_dir =
        cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).map_err(|e| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!("type-stage eval: cannot open ambient dir: {e}"),
                span: origin_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })
        })?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, type_stage_env, false);

    // Allocate materialized thunks for each argument.
    let arg_thunks: Vec<Arc<Thunk>> = args
        .iter()
        .map(|v| Arc::new(Thunk::new_materialized(v.clone(), origin_span.clone())))
        .collect();

    // Dispatch: fn_val must be a user-defined function (not a builtin).
    // Builtin type-stage functions are not expected in as-type dispatch paths.
    // Unwrap Value::Annotated transparently — annotated function bindings (e.g., a
    // type-stage combinator declared with an @[doc: "..."] annotation) carry the function
    // value in `inner`, not at the top level.
    let fn_inner = match fn_val {
        Value::Annotated { inner, .. } => inner.as_ref(),
        other => other,
    };
    let result_thunk = match fn_inner {
        Value::Function {
            ref params,
            ref body,
            env: ref closure_env,
            ..
        } => {
            let call_ctx = crate::eval_call::CallContext {
                params,
                body,
                closure_env,
                positional: &arg_thunks,
                named: None,
                default_env: closure_env,
                call_span: origin_span.clone(),
                origin: None,
                ctx: &ctx,
            };
            crate::eval_call::invoke_function(&call_ctx)
                .await
                .map_err(|e| {
                    TypeErrorTyped::Generic(GenericTypeError {
                        message: format!("type-stage function call failed: {e}"),
                        span: origin_span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    })
                })?
        }
        // Not a function — as-type dispatch requires a callable value.
        _ => {
            return Err(TypeErrorTyped::Generic(GenericTypeError {
                message: "eval_type_stage_value: argument is not a function value".to_string(),
                span: origin_span,
                notes: vec![],
                call_stack: vec![],
            }))
        }
    };

    // Materialize the result.
    let result_val = crate::eval::materialize(&result_thunk, None, &ctx)
        .await
        .map_err(|e| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!("type-stage materialization failed: {e}"),
                span: origin_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })
        })?;

    // Convert TypeNode Value → Type.
    typenode_value_to_type(&result_val, &ctx)
        .await
        .ok_or_else(|| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!("type-stage result cannot be converted to Type: {result_val}"),
                span: origin_span,
                notes: vec![],
                call_stack: vec![],
            })
        })
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
///   → Thunk::new_surface in type-stage env
///   → materialize(...).await   (produces TypeNode Value)
///   → TypeNode.as-type lookup + eval_type_stage_value
///       (normalizes user-defined constructors to primitive TypeNode forms)
///   → typenode_value_to_type
///   → Type
/// ```
///
/// ## Error Behaviour
///
/// Returns `Err(TypeError)` if:
/// - The type-stage environment is unavailable (bootstrap / recursion guard).
/// - Evaluation of the expression fails (runtime error in type-stage code).
/// - The evaluated value cannot be converted to a Type (unrecognized TypeNode tag).
///
/// In the fallback call sites (`resolve_property_dict_as_record`), errors are caught with
/// `.unwrap_or(Type::Unknown)`, preserving existing gradual-typing behaviour for
/// annotations that cannot be resolved to a precise type.
///
/// ## TypeNode.as-type dispatch (T-1059 hook)
///
/// After evaluating the expression, this function looks up `TypeNode` in the type-stage
/// environment and tries to call `TypeNode.as-type` on the result.  The `as-type`
/// protocol function (added to the TypeNode dict by T-1059 via `[merge TypeNode [...]]`)
/// normalizes user-defined TypeNode constructors to primitive forms before conversion.
///
/// If `TypeNode.as-type` is not found in the TypeNode dict (e.g., the merge was not yet
/// evaluated), the raw value is passed directly to `typenode_value_to_type`. Primitive
/// TypeNode constructors (`TypeNode.Int`, `TypeNode.Float`, etc.) are handled without
/// `as-type` dispatch.
pub(crate) async fn eval_type_stage_expr(
    node: &Arc<SurfaceNode>,
    _env: &TypeEnv,
    state: &mut InferState,
) -> Result<Type, TypeError> {
    let node_span = node.span.clone();

    // Obtain the type-stage environment.
    // Prefer state.type_stage_env (set when the source file has --- stage: type sections).
    // Fall back to the prelude type-stage env when state.type_stage_env is None (e.g., for
    // files that use built-in type annotations but declare no type-stage sections of their own).
    let type_stage_env = match state.type_stage_env.clone() {
        Some(env) => env,
        None => match crate::imports::get_prelude_type_stage_env().await {
            Some(env) => env,
            None => return Err(TypeErrorTyped::Generic(GenericTypeError {
                message:
                    "type-stage environment unavailable: prelude type-stage env could not be built"
                        .to_string(),
                span: node_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })),
        },
    };

    // Build a minimal EvalContext backed by the type-stage environment.
    // AMBIENT-OK: type-stage evaluation performs no file I/O.
    #[allow(clippy::disallowed_methods)]
    let base_dir =
        cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).map_err(|e| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!("type-stage eval: cannot open ambient dir: {e}"),
                span: node_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })
        })?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, Arc::clone(&type_stage_env), false);

    // No resolution pass for synthetic type-stage nodes; resolution is inline on nodes
    // (written at definition time). Names resolve via the env chain at eval time.
    // All type annotations are inline on AST nodes — no external tables needed.

    // Wrap the SurfaceNode in a lazy thunk that will evaluate it in the type-stage env.
    let surface_thunk = Arc::new(Thunk::new_surface(
        Arc::clone(node),
        Arc::clone(&type_stage_env),
        Arc::clone(&ctx),
        node_span.clone(),
    ));

    // Materialize — type-stage evaluation is pure compute, no I/O.
    let typenode_val = crate::eval::materialize(&surface_thunk, Some(&node_span), &ctx)
        .await
        .map_err(|e| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!("type-stage expression evaluation failed: {e}"),
                span: node_span.clone(),
                notes: vec![],
                call_stack: vec![],
            })
        })?;

    // Attempt TypeNode.as-type dispatch (T-1059 hook).
    //
    // Look up `TypeNode` in the type-stage environment and retrieve its `as-type` key.
    // If the key exists (after T-1059 adds it via `[merge TypeNode [...]]`), call it with
    // `typenode_val` to normalize user-defined TypeNode constructors to primitive forms.
    // `eval_type_stage_value` calls the function and converts the result to a `Type`.
    //
    // If the lookup fails (pre-T-1059, when `TypeNode.as-type` has not been added to the
    // TypeNode dict by the protocol-functions merge), fall through to direct conversion via
    // `typenode_value_to_type`.  This is correct for T-1060: primitive TypeNode constructors
    // (`TypeNode.Int`, `TypeNode.Float`, etc.) convert without needing `as-type` dispatch.
    // Attempt TypeNode.as-type dispatch (T-1059 hook) using async helpers.
    // We inline the former sync closure as sequential awaited steps so block_on_anywhere
    // is no longer needed. Each step returns Option; on None we fall through to direct
    // conversion via `typenode_value_to_type`.
    let as_type_dispatch: Option<Type> = async {
        // Look up the TypeNode dict in the type-stage environment via slot_names
        // (seeding path — not an eval-time lookup).  Walk slot_names + parent chain
        // to find "TypeNode" without calling get_by_name.
        let typenode_thunk = {
            let mut found: Option<Arc<Thunk>> = None;
            let mut cur: Option<Arc<std::sync::RwLock<crate::value::Environment>>> =
                Some(Arc::clone(&type_stage_env));
            while let Some(frame) = cur {
                let frame_ref = frame.read().ok()?;
                if let Some(idx) = frame_ref.slot_names.iter().rposition(|n| n == "TypeNode") {
                    found = Some(Arc::clone(&frame_ref.slots[idx]));
                    break;
                }
                cur = frame_ref.parent.as_ref().map(Arc::clone);
            }
            found?
        };
        let typenode_dict_val = crate::eval::materialize(&typenode_thunk, None, &ctx)
            .await
            .ok()?;

        // Retrieve the as-type function from the TypeNode dict.
        // This key is added by `[merge TypeNode [...]]` in T-1059; absent until then.
        let as_type_fn = match &typenode_dict_val {
            Value::Dict(ref d) => {
                let as_type_id = d.get(&HashableValue::Str("as-type".into()))?;
                let as_type_thunk = ctx.get_thunk(*as_type_id);
                crate::eval::materialize(&as_type_thunk, None, &ctx)
                    .await
                    .ok()?
            }
            _ => return None,
        };

        // Call TypeNode.as-type(typenode_val) to normalize and convert to Type.
        // eval_type_stage_value handles: call fn → materialize → typenode_value_to_type.
        // Returns None on failure (non-function value, eval error, unrecognized result).
        eval_type_stage_value(&as_type_fn, std::slice::from_ref(&typenode_val), state)
            .await
            .ok()
    }
    .await;

    // If as-type dispatch succeeded, return its result (the normalized Type).
    if let Some(ty) = as_type_dispatch {
        return Ok(ty);
    }

    // Fall through to direct conversion: raw typenode_val → Type (no as-type normalization).
    // Handles TypeNode primitive variants only.
    typenode_value_to_type(&typenode_val, &ctx)
        .await
        .ok_or_else(|| {
            TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "type-stage expression produced an unrecognized value: {typenode_val}"
                ),
                span: node_span,
                notes: vec![],
                call_stack: vec![],
            })
        })
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
        Type::Record(row) => {
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
        Type::NominalVariant { fields, .. } => {
            // B-366: Check both fields.fields AND fields.tail for TyCon refs.
            // Matches the pattern from the Record arm and B-356's apply_type_alias_substitution fix.
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
        Type::Record(row) => {
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
        Type::NominalVariant { fields, .. } => {
            // T-1160: Check both fields.fields AND fields.tail for recursive var refs.
            // Matches the pattern from the Record arm.
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
/// 1. Look up the `TyConDef` for `name` from the `TypeEnv`.
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
/// Returns `None` if the name is not registered in the `TypeEnv`.
///
/// **Cycle origin detection (wrap rule):** after expansion, if the expanded
/// body contains a `TypeVar` matching the pre-assigned binder name, this alias IS the
/// cycle origin — the body is wrapped in `Type::Recursive { var: binder_name, body }`.
/// Non-recursive aliases are returned as-is (no wrapper needed).
///
/// The `Type::Recursive` produced here is consumed by `is_subtype` via the S-Exp + S-Assum
/// coinductive algorithm implemented in S-861. Wiring `expand_named` into the annotation
/// resolver (so that named recursive types reach `is_subtype` at runtime) is deferred to S-862.
pub(crate) fn expand_named(
    name: &str,
    args: &[Type],
    env: &TypeEnv,
    state: &mut InferState,
) -> Option<Type> {
    // Step 1: look up the TyConDef.
    let def_arc = env.lookup_tycon_def(name)?;
    let def = Arc::clone(def_arc);

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

    // Step 2b: fast path for zero-param types with no TyCon references in body.
    // Primitives (Int, Float, etc.) have no params and no TyCon references — return
    // the body directly without pushing to the stack or calling expand_all_tycon_apps.
    if def.params.is_empty() && !body_contains_tycon_ref(&def.body) {
        return Some(def.body.clone());
    }

    // Step 3: builtin-opaque types — do not structurally expand.
    // Return App(TyCon(name), args) so that UNIFY-TYCON handles them by name equality
    // and variance-directed comparison, not by structural equivalence.
    if def.builtin_type.is_some() {
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

    let expanded = expand_all_tycon_apps(&body_substituted, env, state);

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
    // was implemented in S-861 (`is_subtype_inner` in type_def.rs). `Type::Recursive` is
    // produced here and will be consumed by `is_subtype` once `expand_named` is wired into
    // the annotation resolver in S-862.
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
/// - `Type::TyCon(name)` → `expand_named(name, &[], stack, env, state)`
/// - `Type::App(Type::TyCon(name), arg)` → expand arg first, then
///   `expand_named(name, &[expanded_arg], stack, env, state)`
/// - Nested `Type::App(Type::App(TyCon, a), b)` chains → collect all args in order,
///   call `expand_named(name, &[a, b, ...], stack, env, state)` (curried left-assoc)
/// - All other type forms → recurse structurally into children
///
/// Returns the expanded type. Falls back to the original type node when expansion
/// fails (e.g., unknown alias name — the TyCon is preserved for downstream error
/// reporting).
pub(crate) fn expand_all_tycon_apps(ty: &Type, env: &TypeEnv, state: &mut InferState) -> Type {
    match ty {
        // Bare TyCon — zero-arg application.
        Type::TyCon(name) => expand_named(name, &[], env, state).unwrap_or_else(|| ty.clone()),

        // App chain: collect the root TyCon name and all args, then expand.
        // App is left-associative: App(App(TyCon("Map"), Str), Int) = Map[Str][Int].
        // We collect args right-to-left while peeling left-associative Apps, then reverse.
        Type::App(f, arg) => {
            // Expand the argument first (args are always expanded before the ctor).
            let expanded_arg = expand_all_tycon_apps(arg, env, state);

            // Check whether `f` is a TyCon or another App(TyCon, ...) chain.
            // Collect the root name and all preceding args by peeling the App spine.
            let (root_name, preceding_args) = collect_app_spine(f);

            match root_name {
                Some(name) => {
                    // Expand preceding args first, then append the current expanded_arg.
                    let mut all_args: Vec<Type> = preceding_args
                        .iter()
                        .map(|a| expand_all_tycon_apps(a, env, state))
                        .collect();
                    all_args.push(expanded_arg);

                    expand_named(name, &all_args, env, state).unwrap_or_else(|| {
                        // Unknown alias — rebuild the App chain with the expanded args.
                        let base = Type::TyCon(name.to_string());
                        all_args
                            .into_iter()
                            .fold(base, |acc, a| Type::App(Box::new(acc), Box::new(a)))
                    })
                }
                None => {
                    // `f` is not a TyCon chain (e.g., App(TypeVar, arg)) — recurse into f.
                    let expanded_f = expand_all_tycon_apps(f, env, state);
                    Type::App(Box::new(expanded_f), Box::new(expanded_arg))
                }
            }
        }

        // Structural recursion for all other type forms.
        Type::Record(row) => {
            let new_fields: indexmap::IndexMap<String, Type> = row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, env, state)))
                .collect();
            let new_tail = match &row.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, env, state))),
                        value: Box::new(expand_all_tycon_apps(value, env, state)),
                    }
                }
                other => other.clone(),
            };
            Type::Record(Row {
                fields: new_fields,
                tail: new_tail,
            })
        }

        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, p)| (name.clone(), expand_all_tycon_apps(p, env, state)))
                .collect(),
            ret: Box::new(expand_all_tycon_apps(ret, env, state)),
            variadic: *variadic,
            required_count: *required_count,
        },

        Type::Union(members) => Type::normalize_union(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, env, state))
                .collect(),
        ),

        Type::Intersection(members) => Type::normalize_intersection(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, env, state))
                .collect(),
        ),

        Type::Negation(inner) => Type::Negation(Box::new(expand_all_tycon_apps(inner, env, state))),

        Type::NominalVariant { tag, fields } => {
            let new_fields: indexmap::IndexMap<String, Type> = fields
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, env, state)))
                .collect();
            let new_tail = match &fields.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, env, state))),
                        value: Box::new(expand_all_tycon_apps(value, env, state)),
                    }
                }
                other => other.clone(),
            };
            Type::NominalVariant {
                tag: tag.clone(),
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
            body: Box::new(expand_all_tycon_apps(body, env, state)),
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
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
    type_params_scope: Option<(&HashMap<String, crate::types::Type>, bool)>,
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
        return Err(TypeErrorTyped::Generic(GenericTypeError {
            message: format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
            notes: vec![],
            call_stack: vec![],
        }));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(TypeErrorTyped::Generic(GenericTypeError {
            message: "function type parameter list must be auto-indexed".to_string(),
            span: second.span.clone(),
            notes: vec![],
            call_stack: vec![],
        }));
    }

    let ret = resolve_annotation_as_type(
        ann_node,
        env,
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
                env,
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
                    env,
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
            return Err(TypeErrorTyped::Generic(GenericTypeError {
                message: "function type parameter list must be a bracket expression".to_string(),
                span: second.node.value.span.clone(),
                notes: vec![],
                call_stack: vec![],
            }))
        }
    }

    let required_count = params.len();
    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
        variadic: false,
        required_count,
    }))
}
