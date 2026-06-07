//! Type annotation resolution and type expression parsing.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{Annotation, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::type_def::Variance;
use crate::types::{Constraint, InferState, Kind, Row, Type, TypeAlias, TypeEnv, TypeError};
use crate::value::{Key, Thunk, Value};

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
                let passes =
                    Type::is_subtype(&default_ty, &expected_resolved, Some(&state.tycon_env))
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

    Ok(expected)
}

/// Resolve an annotated type expression `[@Name $annotation]`.
///
/// If `name == "Fn"`, interprets `$annotation` as a function type specification:
/// - `[@Fn@RetType [Param1 Param2 ...]]` → function type with params and return type
/// - `[@Fn@RetType]` (no param list) → zero-parameter function returning RetType
///
/// If `name == "Seq"`, interprets `$annotation` as the element type:
/// - `Seq@ElemType` (bare Annotated form) → `Type::seq(elem)`
/// - `[@Seq expr]` (TypeAssert) → checks `expr` against `App(TyCon("Seq"), Any)` (element type is Any; `@ElemType` suffix is a parse error in TypeAssert position)
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
        let ty = Type::seq(elem);
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
        Ok(Type::handle(cap_type))
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
                    Ok(Type::seq(elem_type))
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
                                Ok(Type::map(key_ty, value_ty))
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
                    Ok(Type::handle(cap_type))
                }
                _ => {
                    // Try TyConDef lookup for user-defined parameterized types (T-949).
                    // Handles `@Tree@Int` where Tree is a user-defined TyCon with arity 1.
                    if let Some(def) = env.lookup_tycon_def(name) {
                        if def.arity() >= 1 {
                            // Resolve the inner annotation as the first type argument.
                            let arg = resolve_annotation(
                                inner,
                                env,
                                span,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
                            return Ok(Type::App(
                                Box::new(Type::TyCon(name.clone())),
                                Box::new(arg),
                            ));
                        } else if def.arity() == 0 {
                            // Zero-arity TyCon with annotation — unusual but valid.
                            return Ok(Type::TyCon(name.clone()));
                        }
                    }
                    // Unknown parameterized type — no TyConDef found
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
    resolve_type_dict(
        entries,
        env,
        span.clone(),
        state,
        ann_mapping,
        row_ann_mapping,
    )
    .or_else(|e| {
        if entries_look_like_type_dict(entries) {
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
            Ok(eval_type_stage_expr(&synth_node, env, state).unwrap_or(Type::Unknown))
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
            let new_fields: HashMap<String, Type> = fields
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
/// Also guards against treating builtin names as positional ADT type tags in the
/// single-entry positional path of resolve_type_dict.
///
/// NOTE (T-1087): The original plan was to delete this function once resolve_type_dict was
/// refactored to use TyConDef lookup first. `apply_builtin_constructor` (the other deletion
/// target) has already been removed — its logic was absorbed into the TyConDef-first path
/// at lines 2702–2724. However, `is_builtin_type_name` is still needed at two call sites
/// (nominal constructor disambiguation in resolve_type_dict and VarRef handling) to prevent
/// builtin type names from being parsed as ADT constructor names. Removing this guard requires
/// replacing it with a dynamic check (e.g., `env.lookup_tycon_def(name).is_some()` for registered
/// types, plus `resolve_type_name` fallback for unregistered builtins). That refactor is
/// deferred to a future sprint as it requires careful coordination with `resolve_type_dict`.
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
        "Number" | "Num" => Ok(Type::Number),
        "Bytes" => Ok(Type::Bytes),
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
            let n = state.subst.name_counter.get();
            let fresh = format!("_label_{}", n);
            state.subst.name_counter.set(n.saturating_add(1));
            state.levels.insert(fresh.clone(), state.level);
            state.kind_env.insert(fresh.clone(), crate::types::Kind::Label);
            Ok(Type::TypeVar(fresh, state.level))
        }
        "Seq" => Ok(Type::seq(Type::Unknown)),
        // Bare @Handle — no capability row argument. Resolves to Handle(Unknown),
        // which is the gradual "any handle" type. This is correct for unannotated
        // handle parameters where the caller doesn't know (or care about) the
        // capability row. Parameterized forms (`h@Handle@DirCap`, `[Handle DirCap]`,
        // `@Handle@DirCap`) resolve through resolve_annotated/resolve_annotation/
        // resolve_type_dict respectively and never reach this bare-name path.
        "Handle" => Ok(Type::handle(Type::Unknown)),
        "Null" => Ok(Type::Record(Row {
            fields: HashMap::new(),
            tail: crate::type_def::RowTail::Empty,
        })),
        "Dict" => {
            // Empty record — represents "any dict" under BAS width subtyping.
            // Any concrete record is a subtype because all required fields (none) are present.
            Ok(Type::Record(Row {
                fields: HashMap::new(),
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
                fields: HashMap::new(),
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
                ret: Box::new(Type::Top),
                variadic: true,
            })
        }
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
                // Type parameter scope enforcement (T-951).
                // When inside a TypeAlias body resolution (state.type_params_scope is Some),
                // lowercase names are TypeVars ONLY if they appear in the declared params list.
                // Unknown lowercase names are a type error rather than silently creating a
                // fresh TypeVar — enforcing the "explicit type params" principle.
                if let Some(ref params) = state.type_params_scope {
                    // Check if this name is a declared type param (via ann_mapping which maps
                    // param name → fresh TypeVar name). If not, check if it's a scope reference
                    // (a TyConDef or TypeAlias visible in the current env), else error.
                    let in_params = ann_mapping.as_ref().is_some_and(|m| m.contains_key(name));
                    if !in_params && !params.contains(name) {
                        // Name not declared as a type parameter — check if it's a scope reference.
                        if env.get_type_alias(name).is_none() && env.lookup_tycon_def(name).is_none() {
                            return Err(TypeError::new(
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
            Ok(Type::Record(Row {
                fields: new_fields,
                tail: crate::type_def::RowTail::Empty,
            }))
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
    // Positional-only forms (Fn types, [Seq T], [Map K V], parameterized aliases,
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
                    // Field with annotation: `field@Child: Type` (T-1052).
                    // The annotation is stored in the SurfaceNode for T-1053 to process;
                    // type resolution uses only the field name.
                    SurfaceExpression::Annotated { name, .. } => name.clone(),
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
                            tail: crate::type_def::RowTail::Empty,
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
                            let arg = resolve_type_expr(
                                arg_node,
                                env,
                                state,
                                ann_mapping,
                                row_ann_mapping,
                            )?;
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
                    return instantiate_type_alias(&alias, &type_args, state);
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
                            fields: Row {
                                fields: fields_map,
                                tail: crate::type_def::RowTail::Empty,
                            },
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
                            fields: Row {
                                fields: fields_map,
                                tail: crate::type_def::RowTail::Empty,
                            },
                        });
                    } else if args.is_empty() {
                        // Unit constructor: [None] → NominalVariant("None", {})
                        return Ok(Type::NominalVariant {
                            tag: name.clone(),
                            fields: Row {
                                fields: HashMap::new(),
                                tail: crate::type_def::RowTail::Empty,
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

    // TyConDef-based type constructor application (T-949).
    // Primary path for user-defined and builtin type constructors declared in TyConEnv.
    // Produces left-associative App chains: [Tree Int] → App(TyCon("Tree"), Int).
    // Must run BEFORE the parameterized alias lookup so TyCon constructors take priority.
    //
    // kind_env fallback: handles any builtin TyCons not covered by TyConDef (e.g., future additions).
    // Seq/Map/Handle are registered in TyConDef as of T-1018, but kind_env remains as a safety net.
    if !entries.is_empty() {
        if let Some(first) = entries.first() {
            if first.node.key.is_none() {
                if let SurfaceExpression::VarRef { name, .. } = &first.node.value.expr {
                    if let Some(def) = env.lookup_tycon_def(name) {
                        let arity = def.arity();
                        if arity == 0 && entries.len() == 1 {
                            // Zero-arity TyCon: bare name with no arguments.
                            return Ok(Type::TyCon(name.clone()));
                        } else if arity > 0 {
                            // Collect `arity` argument types from subsequent positional entries.
                            if entries.len() < 1 + arity {
                                return Err(TypeError::new(
                                    format!(
                                        "type constructor '{}' requires {} argument(s), got {}",
                                        name,
                                        arity,
                                        entries.len() - 1
                                    ),
                                    span,
                                ));
                            }
                            let mut result = Type::TyCon(name.clone());
                            for entry in entries.iter().take(arity + 1).skip(1) {
                                let arg = resolve_type_expr(
                                    &entry.node.value,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?;
                                result = Type::App(Box::new(result), Box::new(arg));
                            }
                            // If there are extra entries beyond arity, they remain unused.
                            // (For zero-arity TyCon used as a union member in a multi-entry dict,
                            // the caller's union-detection path handles the full dict.)
                            return Ok(result);
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
                            let args: Result<Vec<Type>, _> = entries[1..]
                                .iter()
                                .map(|e| {
                                    resolve_type_expr(
                                        &e.node.value,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )
                                })
                                .collect();
                            let args = args?;

                            // User-defined: always Kind::Operator (arity 1).
                            // Rank-1 restriction: argument cannot itself be a type constructor.
                            // Note: Seq/Map/Handle are caught by the TyConDef path above and
                            // never reach here (T-1021/T-1018).
                            if args.len() != 1 {
                                return Err(TypeError::new(
                                    format!(
                                        "type constructor `{name}` requires 1 type argument, got {}",
                                        args.len()
                                    ),
                                    span,
                                ));
                            }
                            let a_type = args.into_iter().next().unwrap();
                            if let Type::Operator(op_name) = &a_type {
                                return Err(TypeError::new(
                                    format!(
                                        "kind mismatch: type constructor `{name}` cannot be \
                                         applied to another type constructor `{op_name}`; \
                                         use a concrete type instead"
                                    ),
                                    span,
                                ));
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
                            return instantiate_type_alias(&alias, &type_args, state);
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
                    let is_builtin_type = is_builtin_type_name(&tag);
                    if is_builtin_type && entries.len() == 1 && first.node.key.is_none() {
                        // Single positional entry that is a builtin type name: [Int] → Type::Int.
                        // This handles annotations like @[Int] which should resolve to Int,
                        // not to NominalVariant { tag: "Int" }.
                        let row_ref: Option<&HashMap<String, String>> =
                            row_ann_mapping.as_ref().map(|m| &**m);
                        return resolve_type_name(&tag, env, span, state, ann_mapping, &row_ref);
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
                                        fields: HashMap::new(),
                                        tail: crate::type_def::RowTail::Empty,
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
                            let mut variant_fields = HashMap::new();
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
                    ann_mapping,
                    row_ann_mapping,
                );
            }
        }
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

    // All-positional multi-entry union path: when all entries have no key and don't match
    // any of the specific forms above (constructor, keyword, Fn type, etc.), treat each
    // entry's value as a union member type. This handles TypeAlias bodies like
    // `[type [Foo] [Bar]]` where each `[Foo]` resolves to NominalVariant("Foo", {}).
    // Note: B-314 plans to retire this in favor of explicit `[or ...]` syntax.
    if all_positional && entries.len() >= 2 {
        // Check if any entry has a None key (all positional) — confirmed by all_positional
        // Try resolving each positional entry as a type expression for a union
        let mut members = Vec::new();
        let mut all_ok = true;
        for entry in entries {
            if entry.node.key.is_none() {
                match resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)
                {
                    Ok(ty) => members.push(ty),
                    Err(_) => {
                        all_ok = false;
                        break;
                    }
                }
            } else {
                all_ok = false;
                break;
            }
        }
        if all_ok && !members.is_empty() {
            return Ok(Type::normalize_union(members));
        }
    }

    let mut fields: HashMap<String, Type> = HashMap::new();
    let mut has_rest = false; // tracks if `...` is present (BAS: openness via width subtyping)
                              // Column constraint: `{_ : V}` or `{_@K : V}` annotation syntax (T-950).
                              // At most one `_` per row type; duplicate produces a type error.
    let mut uniform_tail: Option<crate::type_def::RowTail> = None;

    for entry in entries {
        if let SurfaceExpression::Rest(_name) = &entry.node.value.expr {
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
                return Err(TypeError::new(
                    "duplicate uniform-field sentinel `_` in row type annotation — at most one `_` allowed per row",
                    entry.span.clone(),
                ));
            }
            let value_ty =
                resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
            // Check for typed-key form `_@K` vs plain `_`
            let key_ty = match entry.node.key.as_ref().map(|k| &k.expr) {
                Some(SurfaceExpression::Annotated { annotation, .. }) => {
                    // `_@K`: resolve K as the key type constraint.
                    let key_t = resolve_annotation(
                        &annotation.node,
                        env,
                        annotation.span.clone(),
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
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
                    let mut member_fields = HashMap::new();
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
        Arc::new(SurfaceNode {
            expr: SurfaceExpression::Call {
                func,
                args,
                named_args: vec![],
                implied: true,
            },
            span,
        })
    } else {
        Arc::new(SurfaceNode {
            expr: SurfaceExpression::Dict(entries.to_vec()),
            span,
        })
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
fn variant_payload_dict(
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
    let payload_val = crate::eval::materialize_sync(&payload_thunk, None, ctx).ok()?;
    match payload_val {
        Value::Dict(dict) => {
            // Extract each string-keyed field, materializing the field value.
            let mut fields = HashMap::new();
            for (key, thunk_id) in &dict {
                if let Key::String(k) = key {
                    let field_thunk = ctx.get_thunk(*thunk_id);
                    if let Ok(v) = crate::eval::materialize_sync(&field_thunk, None, ctx) {
                        fields.insert(k.to_string(), v);
                    }
                }
            }
            Some(fields)
        }
        _ => None,
    }
}

/// Collect a tinct Seq (lazy linked list of `Seq.Cons`/`Seq.Nil` Variants) into a Vec.
///
/// Each element is passed through `typenode_value_to_type` to convert it to a `Type`.
/// Returns `None` if any element fails to convert or the spine is malformed.
///
/// Used by `typenode_value_to_type` to process `TypeNode.Union.types`,
/// `TypeNode.Intersect.types`, `TypeNode.Arrow.params`, and `TypeNode.TypeApplication.args`.
fn collect_typenode_seq(seq_val: Value, ctx: &Arc<crate::eval::EvalContext>) -> Option<Vec<Type>> {
    let mut result = Vec::new();
    let mut current = seq_val;
    // Depth limit to guard against malformed cycles (TypeNode seqs are always finite).
    let mut depth = 0usize;
    const MAX_SEQ_DEPTH: usize = 256;

    loop {
        if depth > MAX_SEQ_DEPTH {
            return None;
        }
        depth += 1;

        // Determine the next action without holding a borrow on `current` across
        // the assignment `current = ...` below. We first extract the information we
        // need (tag, payload_id) from `current`, then drop the borrow before mutating.
        enum SeqStep {
            Nil,
            Cons(crate::value::ThunkId), // payload ThunkId from Seq.Cons
            CollectedDict,               // already-collected integer-keyed dict
            Annotated,                   // Value::Annotated wrapper
            Unknown,                     // unrecognised shape
        }
        let step = match &current {
            Value::Variant { tag, payload: None } if tag == "Seq.Nil" => SeqStep::Nil,
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => SeqStep::Cons(*payload_id),
            Value::Dict(_) => SeqStep::CollectedDict,
            Value::Annotated { .. } => SeqStep::Annotated,
            _ => SeqStep::Unknown,
        };

        match step {
            SeqStep::Nil => {
                // End of sequence — return collected elements.
                return Some(result);
            }
            SeqStep::Cons(payload_id) => {
                // Materialize the Seq.Cons payload dict to extract head and tail.
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = crate::eval::materialize_sync(&payload_thunk, None, ctx).ok()?;
                let dict = match payload_val {
                    Value::Dict(d) => d,
                    _ => return None,
                };
                // Extract and convert the head element.
                let head_id = *dict.get(&Key::String("head".into()))?;
                let head_thunk = ctx.get_thunk(head_id);
                let head_val = crate::eval::materialize_sync(&head_thunk, None, ctx).ok()?;
                let head_ty = typenode_value_to_type(&head_val, ctx)?;
                result.push(head_ty);
                // Advance to tail (replaces `current` for the next iteration).
                let tail_id = *dict.get(&Key::String("tail".into()))?;
                let tail_thunk = ctx.get_thunk(tail_id);
                current = crate::eval::materialize_sync(&tail_thunk, None, ctx).ok()?;
            }
            SeqStep::CollectedDict => {
                // Integer-keyed dict (result of builtin-collect on a Seq) — iterate in order.
                // Ownership of `current` is needed; extract the dict now.
                let dict = match current {
                    Value::Dict(d) => d,
                    _ => return None,
                };
                if dict.is_empty() {
                    // Empty dict is the terminal Seq value (Seq.Nil equivalent).
                    return Some(result);
                }
                let mut i = 0i64;
                loop {
                    match dict.get(&Key::Int(i)) {
                        Some(thunk_id) => {
                            let thunk = ctx.get_thunk(*thunk_id);
                            let val = crate::eval::materialize_sync(&thunk, None, ctx).ok()?;
                            let ty = typenode_value_to_type(&val, ctx)?;
                            result.push(ty);
                            i += 1;
                        }
                        None => return Some(result),
                    }
                }
            }
            SeqStep::Annotated => {
                // Unwrap Value::Annotated and retry with the inner value.
                current = match current {
                    Value::Annotated { inner, .. } => inner.as_ref().clone(),
                    _ => return None,
                };
            }
            SeqStep::Unknown => return None,
        }
    }
}

/// Convert a TypeNode `Value` to a `Type`.
///
/// Handles two formats produced by the type-stage evaluator:
///
/// 1. **Old-style kind-keyed dicts** — `{kind: "named", name: "Int"}`. The prelude
///    combinators are fully migrated (T-1061), but user code may still construct kind-keyed
///    dicts directly. Delegated to `crate::type_normalize::dict_to_type` as a fallback.
///    Will be retired when `dict_to_type` is removed in a future cleanup sprint.
///
/// 2. **TypeNode Variant values** — `Variant { tag: "TypeNode.Int" }`, `Variant { tag:
///    "TypeNode.Union", payload: ... }` etc., produced by the TypeNode ADT declared in the
///    type-stage prelude (T-1058/T-1061). Matched by tag prefix `"TypeNode."`.
///
/// Returns `None` if the value cannot be recognized as a Type.
///
/// **Coverage:** All structural TypeNode variants are handled: Union, Intersect, Record,
/// Arrow, TypeConstructor, TypeApplication, TypeVar. TypeNode.Recursive and
/// TypeNode.RecursiveRef return `None` — they require the equirecursive CheckerType migration
/// and are deferred to a future sprint.
///
/// Public (crate-internal) re-export for use by `type_normalize::evaluate_resolver`.
/// The implementation is `typenode_value_to_type`; this wrapper adds the `pub(crate)` visibility.
pub(crate) fn typenode_value_to_type_pub(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Type> {
    typenode_value_to_type(val, ctx)
}

fn typenode_value_to_type(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Option<Type> {
    match val {
        // Old-style kind-keyed dict fallback (type_normalize::dict_to_type).
        // T-1061: prelude combinators are fully migrated to TypeNode Variants.
        // This path handles any user code that still constructs kind:-keyed dicts
        // directly (e.g. `[kind: "named" name: "Int"]`). It will be retired when
        // dict_to_type is removed in a future cleanup sprint.
        Value::Dict(_) => crate::type_normalize::dict_to_type(val, ctx),

        // Annotated wrapper — TypeNode constructors with @[...] annotations on their
        // constructor names (e.g. `[Int@[as-type: [fn [let t] t]  guarding: true]]`) are
        // wrapped in Value::Annotated at runtime. The inner value is the bare Variant;
        // the annotation dict carries the constructor metadata. Unwrap transparently.
        Value::Annotated { inner, .. } => typenode_value_to_type(inner, ctx),

        // TypeNode Variant values produced by the TypeNode ADT (T-1058 / T-1061).
        Value::Variant { tag, payload: _ } => {
            match tag.as_str() {
                // ── Primitive leaf constructors ──────────────────────────────────────
                // No payload — map directly to concrete Type variants.
                "TypeNode.Int" => Some(Type::Int),
                "TypeNode.Float" => Some(Type::Float),
                "TypeNode.String" => Some(Type::Str),
                "TypeNode.Bool" => Some(Type::Bool),
                "TypeNode.Unknown" => Some(Type::Unknown),
                "TypeNode.Never" => Some(Type::Never),
                "TypeNode.Absent" => Some(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                })),

                // ── Union ─────────────────────────────────────────────────────────────
                // TypeNode.Union { types: [Seq TypeNode] } → Type::normalize_union(members)
                "TypeNode.Union" => {
                    let fields = variant_payload_dict(val, ctx)?;
                    let types_val = fields.get("types")?.clone();
                    let members = collect_typenode_seq(types_val, ctx)?;
                    if members.is_empty() {
                        return None; // Empty union is ill-formed — fall back to Unknown.
                    }
                    Some(Type::normalize_union(members))
                }

                // ── Intersect ────────────────────────────────────────────────────────
                // TypeNode.Intersect { types: [Seq TypeNode] } → Type::normalize_intersection(members)
                "TypeNode.Intersect" => {
                    let fields = variant_payload_dict(val, ctx)?;
                    let types_val = fields.get("types")?.clone();
                    let members = collect_typenode_seq(types_val, ctx)?;
                    if members.is_empty() {
                        return None; // Empty intersection is ill-formed — fall back to Unknown.
                    }
                    Some(Type::normalize_intersection(members))
                }

                // ── Record ───────────────────────────────────────────────────────────
                // TypeNode.Record { fields: [Map String TypeNode], open: Bool }
                // → Type::Record(Row { fields: HashMap<String, Type>, tail: Empty | Uniform })
                "TypeNode.Record" => {
                    let payload_fields = variant_payload_dict(val, ctx)?;
                    let fields_val = payload_fields.get("fields")?.clone();
                    let open_val = payload_fields.get("open")?.clone();

                    // `fields` is a Dict (Map String TypeNode) — string-keyed, values are TypeNodes.
                    let record_fields = match fields_val {
                        Value::Dict(ref dict) => {
                            let mut out: HashMap<String, Type> = HashMap::new();
                            for (key, thunk_id) in dict {
                                if let Key::String(k) = key {
                                    let thunk = ctx.get_thunk(*thunk_id);
                                    let v =
                                        crate::eval::materialize_sync(&thunk, None, ctx).ok()?;
                                    let ty = typenode_value_to_type(&v, ctx)?;
                                    out.insert(k.to_string(), ty);
                                }
                            }
                            out
                        }
                        // Empty dict / Seq.Nil for empty record
                        Value::Variant { tag, payload: None } if tag == "Seq.Nil" => HashMap::new(),
                        _ => HashMap::new(),
                    };

                    let tail = if matches!(open_val, Value::Bool(true)) {
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
                    let fields = variant_payload_dict(val, ctx)?;
                    let params_val = fields.get("params")?.clone();
                    let result_val = fields.get("result")?.clone();

                    let param_types = collect_typenode_seq(params_val, ctx)?;
                    let ret_type = typenode_value_to_type(&result_val, ctx)?;

                    let params: Vec<(Option<String>, Type)> =
                        param_types.into_iter().map(|t| (None, t)).collect();

                    Some(Type::Function {
                        params,
                        ret: Box::new(ret_type),
                        variadic: false,
                    })
                }

                // ── TypeConstructor ───────────────────────────────────────────────────
                // TypeNode.TypeConstructor { name: String }
                // Bare (transient): name without '.' → Type::TyCon(name) for expansion
                // Qualified (leaf): name with '.' → Type::NominalVariant or TyCon leaf
                "TypeNode.TypeConstructor" => {
                    let fields = variant_payload_dict(val, ctx)?;
                    let name_val = fields.get("name")?;
                    let name = name_val.as_str()?.to_string();
                    // Map known primitive names to their concrete Type variants.
                    // (These arise when param-token TypeConstructors from parametric bodies
                    // are passed to typenode_value_to_type without being substituted first.)
                    match name.as_str() {
                        "Int" => Some(Type::Int),
                        "Float" => Some(Type::Float),
                        "String" | "Str" => Some(Type::Str),
                        "Bool" => Some(Type::Bool),
                        "Unknown" => Some(Type::Unknown),
                        "Never" => Some(Type::Never),
                        "Absent" => Some(Type::Record(Row {
                            fields: HashMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        })),
                        _ => Some(Type::TyCon(name)),
                    }
                }

                // ── TypeApplication ───────────────────────────────────────────────────
                // TypeNode.TypeApplication { ctor: TypeNode, args: [Seq TypeNode] }
                // → left-associative Type::App chain: App(App(ctor, args[0]), args[1])...
                "TypeNode.TypeApplication" => {
                    let fields = variant_payload_dict(val, ctx)?;
                    let ctor_val = fields.get("ctor")?.clone();
                    let args_val = fields.get("args")?.clone();

                    let ctor_type = typenode_value_to_type(&ctor_val, ctx)?;
                    let arg_types = collect_typenode_seq(args_val, ctx)?;

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
                    let fields = variant_payload_dict(val, ctx)?;
                    let name_val = fields.get("name")?;
                    let level_val = fields.get("level")?;
                    let name = name_val.as_str()?.to_string();
                    let level = match level_val {
                        Value::Int(n) => *n as u32,
                        _ => 0u32,
                    };
                    Some(Type::TypeVar(name, level))
                }

                // ── Recursive / RecursiveRef ──────────────────────────────────────────
                // These require the CheckerType migration (equirecursive types full impl).
                // Return None so the caller falls back gracefully to Type::Unknown.
                "TypeNode.Recursive" | "TypeNode.RecursiveRef" => None,

                // Unknown tag — not a recognized TypeNode constructor.
                _ => None,
            }
        }

        // Not a Dict, Annotated, or Variant — cannot be a TypeNode value.
        _ => None,
    }
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
/// - The type-stage environment is unavailable (build_type_stage_env returned None).
/// - Function invocation or materialization fails.
/// - The result cannot be converted to a `Type` (TypeNode.Recursive/RecursiveRef require the
///   equirecursive CheckerType migration — they return `None` and cause this error).
///
/// ## Usage
///
/// Called from `eval_type_stage_expr` to apply `TypeNode.as-type` normalization after
/// evaluating an annotation expression. Also callable directly when the function value is
/// already in hand (e.g., an `as-type:` fn extracted from a constructor annotation).
///
/// `state` is accepted for API consistency with future callers that will use it for level
/// tracking once the equirecursive CheckerType migration is complete. Not used by the
/// current implementation.
pub(crate) fn eval_type_stage_value(
    fn_val: &Value,
    args: &[Value],
    _state: &mut InferState,
) -> Result<Type, TypeError> {
    let origin_span = crate::ast::Span::origin();

    // Obtain the type-stage environment for building the EvalContext.
    let type_stage_env = crate::imports::build_type_stage_env().ok_or_else(|| {
        TypeError::new(
            "type-stage environment unavailable (bootstrap recursion guard fired)",
            origin_span.clone(),
        )
    })?;

    // Build a minimal EvalContext backed by the type-stage environment.
    // AMBIENT-OK: type-stage evaluation performs no file I/O.
    #[allow(clippy::disallowed_methods)]
    let base_dir =
        cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).map_err(|e| {
            TypeError::new(
                format!("type-stage eval: cannot open ambient dir: {e}"),
                origin_span.clone(),
            )
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
            crate::eval_call::invoke_function_sync(&call_ctx).map_err(|e| {
                TypeError::new(
                    format!("type-stage function call failed: {e}"),
                    origin_span.clone(),
                )
            })?
        }
        // Not a function — as-type dispatch requires a callable value.
        _ => {
            return Err(TypeError::new(
                "eval_type_stage_value: argument is not a function value",
                origin_span,
            ))
        }
    };

    // Materialize the result synchronously.
    let result_val = crate::eval::materialize_sync(&result_thunk, None, &ctx).map_err(|e| {
        TypeError::new(
            format!("type-stage materialization failed: {e}"),
            origin_span.clone(),
        )
    })?;

    // Convert TypeNode Value → Type.
    typenode_value_to_type(&result_val, &ctx).ok_or_else(|| {
        TypeError::new(
            format!(
                "type-stage result cannot be converted to Type: {result_val} \
                 (TypeNode.Recursive/RecursiveRef require equirecursive CheckerType migration)"
            ),
            origin_span,
        )
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
///   → materialize_sync   (produces TypeNode Value)
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
/// - The evaluated value cannot be converted to a Type (only `TypeNode.Recursive` and
///   `TypeNode.RecursiveRef` fall into this category — they require the equirecursive
///   CheckerType migration and are deferred to a future sprint).
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
/// TypeNode constructors (`TypeNode.Int`, `TypeNode.Float`, etc.) and kind-keyed dicts
/// are both handled without `as-type` dispatch.
pub(crate) fn eval_type_stage_expr(
    node: &Arc<SurfaceNode>,
    _env: &TypeEnv,
    state: &mut InferState,
) -> Result<Type, TypeError> {
    let node_span = node.span.clone();

    // Obtain the type-stage environment.
    let type_stage_env = crate::imports::build_type_stage_env().ok_or_else(|| {
        TypeError::new(
            "type-stage environment unavailable (bootstrap recursion guard fired)",
            node_span.clone(),
        )
    })?;

    // Build a minimal EvalContext backed by the type-stage environment.
    // AMBIENT-OK: type-stage evaluation performs no file I/O.
    #[allow(clippy::disallowed_methods)]
    let base_dir =
        cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).map_err(|e| {
            TypeError::new(
                format!("type-stage eval: cannot open ambient dir: {e}"),
                node_span.clone(),
            )
        })?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, Arc::clone(&type_stage_env), false);

    // Create empty resolution and annotation tables (no prior resolution pass for
    // synthetic type-stage nodes; names are resolved at eval time from the env chain).
    let res_table = crate::ast::empty_resolution_table_arc();
    let types_table = crate::ast::empty_type_annotation_table_arc();

    // Wrap the SurfaceNode in a lazy thunk that will evaluate it in the type-stage env.
    let surface_thunk = Arc::new(Thunk::new_surface(
        Arc::clone(node),
        res_table,
        types_table,
        Arc::clone(&type_stage_env),
        Arc::clone(&ctx),
        node_span.clone(),
    ));

    // Materialize synchronously — type-stage evaluation is pure compute, no I/O.
    let typenode_val = crate::eval::materialize_sync(&surface_thunk, Some(&node_span), &ctx)
        .map_err(|e| {
            TypeError::new(
                format!("type-stage expression evaluation failed: {e}"),
                node_span.clone(),
            )
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
    // (`TypeNode.Int`, `TypeNode.Float`, etc.) and kind-keyed dicts both convert without
    // needing `as-type` dispatch.
    let as_type_dispatch: Option<Type> = (|| {
        // Look up the TypeNode dict in the type-stage environment.
        let typenode_thunk = type_stage_env.read().ok()?.get("TypeNode")?;
        let typenode_dict_val = crate::eval::materialize_sync(&typenode_thunk, None, &ctx).ok()?;

        // Retrieve the as-type function from the TypeNode dict.
        // This key is added by `[merge TypeNode [...]]` in T-1059; absent until then.
        let as_type_fn = match &typenode_dict_val {
            Value::Dict(ref d) => {
                let as_type_id = d.get(&Key::String("as-type".into()))?;
                let as_type_thunk = ctx.get_thunk(*as_type_id);
                crate::eval::materialize_sync(&as_type_thunk, None, &ctx).ok()?
            }
            _ => return None,
        };

        // Call TypeNode.as-type(typenode_val) to normalize and convert to Type.
        // eval_type_stage_value handles: call fn → materialize → typenode_value_to_type.
        // Returns None on failure (non-function value, eval error, unrecognized result).
        eval_type_stage_value(&as_type_fn, std::slice::from_ref(&typenode_val), state).ok()
    })();

    // If as-type dispatch succeeded, return its result (the normalized Type).
    if let Some(ty) = as_type_dispatch {
        return Ok(ty);
    }

    // Fall through to direct conversion: raw typenode_val → Type (no as-type normalization).
    // Handles TypeNode primitive variants and old-style kind-keyed dicts.
    typenode_value_to_type(&typenode_val, &ctx).ok_or_else(|| {
        TypeError::new(
            format!(
                "type-stage expression produced an unrecognized value: {typenode_val} \
                 (TypeNode.Recursive/RecursiveRef require equirecursive CheckerType migration)"
            ),
            node_span,
        )
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
// Scaffolding for equirecursive types (S-860 CheckerType migration)
#[allow(dead_code)]
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
#[allow(dead_code)] // S-860 CheckerType migration will use this
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
#[allow(dead_code)] // S-860 CheckerType migration
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
#[allow(dead_code)] // S-860 scaffolding — wired into annotation resolver in S-862
pub(crate) fn expand_named(
    name: &str,
    args: &[Type],
    stack: &mut ExpansionStack,
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
    for (stack_def, binder_name) in stack.iter() {
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
    stack.push((def, binder_name.clone()));

    let expanded = expand_all_tycon_apps(&body_substituted, stack, env, state);

    stack.pop();

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
#[allow(dead_code)] // S-860 CheckerType migration
pub(crate) fn expand_all_tycon_apps(
    ty: &Type,
    stack: &mut ExpansionStack,
    env: &TypeEnv,
    state: &mut InferState,
) -> Type {
    match ty {
        // Bare TyCon — zero-arg application.
        Type::TyCon(name) => {
            expand_named(name, &[], stack, env, state).unwrap_or_else(|| ty.clone())
        }

        // App chain: collect the root TyCon name and all args, then expand.
        // App is left-associative: App(App(TyCon("Map"), Str), Int) = Map[Str][Int].
        // We collect args right-to-left while peeling left-associative Apps, then reverse.
        Type::App(f, arg) => {
            // Expand the argument first (args are always expanded before the ctor).
            let expanded_arg = expand_all_tycon_apps(arg, stack, env, state);

            // Check whether `f` is a TyCon or another App(TyCon, ...) chain.
            // Collect the root name and all preceding args by peeling the App spine.
            let (root_name, preceding_args) = collect_app_spine(f);

            match root_name {
                Some(name) => {
                    // Expand preceding args first, then append the current expanded_arg.
                    let mut all_args: Vec<Type> = preceding_args
                        .iter()
                        .map(|a| expand_all_tycon_apps(a, stack, env, state))
                        .collect();
                    all_args.push(expanded_arg);

                    expand_named(name, &all_args, stack, env, state).unwrap_or_else(|| {
                        // Unknown alias — rebuild the App chain with the expanded args.
                        let base = Type::TyCon(name.to_string());
                        all_args
                            .into_iter()
                            .fold(base, |acc, a| Type::App(Box::new(acc), Box::new(a)))
                    })
                }
                None => {
                    // `f` is not a TyCon chain (e.g., App(TypeVar, arg)) — recurse into f.
                    let expanded_f = expand_all_tycon_apps(f, stack, env, state);
                    Type::App(Box::new(expanded_f), Box::new(expanded_arg))
                }
            }
        }

        // Structural recursion for all other type forms.
        Type::Record(row) => {
            let new_fields: HashMap<String, Type> = row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, stack, env, state)))
                .collect();
            let new_tail = match &row.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, stack, env, state))),
                        value: Box::new(expand_all_tycon_apps(value, stack, env, state)),
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
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, p)| (name.clone(), expand_all_tycon_apps(p, stack, env, state)))
                .collect(),
            ret: Box::new(expand_all_tycon_apps(ret, stack, env, state)),
            variadic: *variadic,
        },

        Type::Union(members) => Type::normalize_union(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, stack, env, state))
                .collect(),
        ),

        Type::Intersection(members) => Type::normalize_intersection(
            members
                .iter()
                .map(|m| expand_all_tycon_apps(m, stack, env, state))
                .collect(),
        ),

        Type::Negation(inner) => {
            Type::Negation(Box::new(expand_all_tycon_apps(inner, stack, env, state)))
        }

        Type::NominalVariant { tag, fields } => {
            let new_fields: HashMap<String, Type> = fields
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), expand_all_tycon_apps(v, stack, env, state)))
                .collect();
            let new_tail = match &fields.tail {
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key
                            .as_ref()
                            .map(|k| Box::new(expand_all_tycon_apps(k, stack, env, state))),
                        value: Box::new(expand_all_tycon_apps(value, stack, env, state)),
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
            body: Box::new(expand_all_tycon_apps(body, stack, env, state)),
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
#[allow(dead_code)] // S-860 CheckerType migration
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

// ─────────────────────────────────────────────────────────────────────────────
// CheckerType — thin Value wrapper for TypeNode representation (T-1070, S-860)
// ─────────────────────────────────────────────────────────────────────────────

/// A thin newtype wrapper around a [`Value`] that is a TypeNode variant.
///
/// `CheckerType` is the scaffolding for the equirecursive type system migration
/// (equirecursive-types-core, S-860). It wraps a tinct `Value` — specifically a
/// `Value::Variant` whose tag is one of the `TypeNode.*` constructor names — so
/// that the type checker can operate on TypeNode values directly instead of the
/// Rust `Type` enum.
///
/// ## Design rationale
///
/// As specified in `doc/whatif/equirecursive-types.md §CheckerType`, the long-term
/// representation of all types (including inference variables) is a TypeNode tinct
/// value. `TypeVar` is `TypeNode.TypeVar name: String  level: Int` — a first-class
/// TypeNode constructor, not a separate Rust variant. This eliminates the parallel
/// `Type` enum representation and makes TypeNode values directly walkable by
/// `TypeNode.children` without any Rust-side dispatch.
///
/// ## Current state (S-860/S-861 scaffolding)
///
/// This struct and its associated functions are not yet wired into the active
/// type-checking pipeline. S-862 will:
/// - Migrate annotation resolution to produce `CheckerType` instead of `Type`
/// - Give the type-checker arena access so `fresh_type_var` and payload-carrying
///   conversions in `from_type` can construct proper
///   `Value::Variant { payload: Some(ThunkId) }` dicts
/// - Retire the parallel `Type` enum
///
/// Until S-862, all payload-carrying TypeNode conversions fall back to
/// `TypeNode.Unknown`. Leaf (unit) constructors are fully correct: they produce
/// `Value::Variant { tag: "TypeNode.X", payload: None }` which requires no arena.
///
/// ## Relationship to `InferState.levels`
///
/// `InferState.levels: HashMap<String, u32>` remains the **authoritative** current
/// level for each type variable (Kiselyov 2013). The `level` field embedded in a
/// `TypeNode.TypeVar` payload is the **creation-time** level — fixed at
/// `fresh_type_var` call time. Level lowering updates `state.levels[name]`, never
/// the payload. All generalization and level-comparison code MUST read
/// `state.levels`, not the payload. See `doc/whatif/equirecursive-types.md
/// §TypeNode` ("collect_type_vars reads level from state.levels[name]").
#[allow(dead_code)] // S-860 scaffolding — wired in S-862
#[derive(Clone, Debug)]
pub(crate) struct CheckerType(pub(crate) Value);

impl CheckerType {
    /// Construct a `CheckerType` for a unit (payload-free) TypeNode variant.
    ///
    /// Internal helper used by `from_type` and the primitive leaf conversions.
    fn unit_variant(tag: &'static str) -> Self {
        CheckerType(Value::Variant {
            tag: tag.to_string(),
            payload: None,
        })
    }

    /// Convert a `Type` enum value to its `CheckerType` (TypeNode Value) equivalent.
    ///
    /// ## Conversion table
    ///
    /// | `Type` variant        | TypeNode tag             | Notes                                  |
    /// |-----------------------|--------------------------|----------------------------------------|
    /// | `Int`                 | `TypeNode.Int`           | leaf, correct                          |
    /// | `IntLiteral(_)`       | `TypeNode.Int`           | promoted to Int (no literal TypeNode)  |
    /// | `Float`               | `TypeNode.Float`         | leaf, correct                          |
    /// | `Str`                 | `TypeNode.String`        | leaf, correct                          |
    /// | `StringLiteral(_)`    | `TypeNode.String`        | promoted to String                     |
    /// | `Bool`                | `TypeNode.Bool`          | leaf, correct                          |
    /// | `Unknown`             | `TypeNode.Unknown`       | leaf, correct                          |
    /// | `Never`               | `TypeNode.Never`         | leaf, correct                          |
    /// | `TypeVar(name, lvl)`  | `TypeNode.Unknown`       | TODO(S-862): needs arena for payload   |
    /// | `Function { .. }`     | `TypeNode.Unknown`       | TODO(S-862): needs arena for payload   |
    /// | `Record(_)`           | `TypeNode.Unknown`       | TODO(S-862): needs arena for payload   |
    /// | `Union(_)`            | `TypeNode.Unknown`       | TODO(S-862): needs arena for payload   |
    /// | all others            | `TypeNode.Unknown`       | TODO(S-862): map remaining variants    |
    ///
    /// ## Why payload-carrying types fall back to `TypeNode.Unknown`
    ///
    /// `Value::Variant { payload: Some(ThunkId) }` requires insertion into a `ThunkArena`
    /// (via `EvalContext::alloc_thunk`). The type-checker does not currently hold arena
    /// access — arenas are owned by the evaluation pipeline (`EvalContext`). Until S-862
    /// threads arena access into the type-checker (or introduces a dedicated checker-local
    /// arena), we cannot construct well-formed payload dicts. Returning `TypeNode.Unknown`
    /// is safe for scaffolding: callers in S-862 will replace these paths with fully-wired
    /// arena-backed constructors.
    ///
    /// `_state: &InferState` is accepted now (unused) so S-862 can add level lookups
    /// without changing the signature.
    #[allow(dead_code)] // S-860 scaffolding — wired in S-862
    pub(crate) fn from_type(ty: &Type, _state: &InferState) -> Self {
        match ty {
            // ── Leaf primitives — no payload required ─────────────────────────
            Type::Int | Type::IntLiteral(_) => Self::unit_variant("TypeNode.Int"),
            Type::Float => Self::unit_variant("TypeNode.Float"),
            Type::Str | Type::StringLiteral(_) => Self::unit_variant("TypeNode.String"),
            Type::Bool => Self::unit_variant("TypeNode.Bool"),
            Type::Unknown => Self::unit_variant("TypeNode.Unknown"),
            Type::Never => Self::unit_variant("TypeNode.Never"),

            // ── Payload-carrying types — TODO(S-862) ──────────────────────────
            // These variants require a ThunkArena to construct their payload dicts.
            // S-862 will thread arena access into the type-checker and complete these arms.
            //
            // Intended final forms:
            //   TypeVar(name, lvl) → TypeNode.TypeVar { name: name, level: state.levels[name] }
            //   Function { .. }    → TypeNode.Arrow { params: Seq<TypeNode>, result: TypeNode }
            //   Record(row)        → TypeNode.Record { fields: Map<String,TypeNode>, open: Bool }
            //   Union(members)     → TypeNode.Union { types: Seq<TypeNode> }
            Type::TypeVar(_, _) | Type::Function { .. } | Type::Record(_) | Type::Union(_) => {
                Self::unit_variant("TypeNode.Unknown")
            }

            // ── Everything else — Unknown fallback ────────────────────────────
            // Covers: Number, Bytes, Top, Error, DirCap, NetCap, Uri, Timestamp,
            // Duration, ClockCap, Timezone, QuicSession, Http2Session, Http3Session,
            // QuicDatagramHandle, DatagramHandle, Intersection, Negation, App, TyCon,
            // Operator, TypeStageApp, NominalVariant, Proxy.
            // TODO(S-862): map remaining variants to their TypeNode equivalents.
            _ => Self::unit_variant("TypeNode.Unknown"),
        }
    }

    /// Create a `CheckerType` representing a fresh inference type variable.
    ///
    /// ## Intended final form (S-861)
    ///
    /// ```text
    /// TypeNode.TypeVar { name: name, level: level }
    /// ```
    /// as `Value::Variant { tag: "TypeNode.TypeVar", payload: Some(ThunkId) }` where
    /// the payload is a `Value::Dict { "name" → String(name), "level" → Int(level) }`.
    ///
    /// Constructing the payload dict requires inserting it into a `ThunkArena` — which
    /// the type-checker does not currently hold. S-862 will add arena access and complete
    /// this constructor.
    ///
    /// ## Current stub (S-860)
    ///
    /// Returns `TypeNode.Unknown` (a unit variant, no arena needed) as a placeholder.
    /// `_name` and `_level` are accepted so S-862 can fill in the implementation without
    /// changing call sites.
    ///
    /// ## `state.levels` remains authoritative
    ///
    /// This function does NOT touch `InferState.levels`. `state.levels[name]` is the
    /// authoritative current level for the variable (Kiselyov 2013 level lowering).
    /// The `level` argument here is the creation-time level to embed in the TypeNode
    /// payload; `state.levels` may lower it subsequently. All level comparisons during
    /// generalization MUST read `state.levels`, not this embedded value.
    #[allow(dead_code)] // S-860 scaffolding — wired in S-862
    pub(crate) fn fresh_type_var(_name: String, _level: u32) -> Self {
        // TODO(S-862): Replace with:
        //
        //   use indexmap::IndexMap;
        //   use crate::value::Key;
        //   use crate::ast::Span;
        //   use std::sync::Arc;
        //
        //   let mut dict = IndexMap::new();
        //   dict.insert(Key::String("name".into()),  /* alloc String thunk into arena */);
        //   dict.insert(Key::String("level".into()), /* alloc Int thunk into arena */);
        //   let payload_id = arena.alloc(Arc::new(
        //       Thunk::new_materialized(Value::Dict(dict), Span::origin())
        //   ));
        //   CheckerType(Value::Variant {
        //       tag: "TypeNode.TypeVar".to_string(),
        //       payload: Some(payload_id),
        //   })
        Self::unit_variant("TypeNode.Unknown")
    }
}

#[cfg(test)]
mod checker_type_tests {
    use super::*;

    /// Helper: extract the TypeNode variant tag from a `CheckerType`.
    fn typenode_tag(ct: &CheckerType) -> &str {
        match &ct.0 {
            Value::Variant { tag, .. } => tag.as_str(),
            other => panic!("CheckerType does not wrap a Variant: {:?}", other),
        }
    }

    /// Helper: verify the CheckerType wraps a unit variant (payload is None).
    fn is_unit_variant(ct: &CheckerType) -> bool {
        matches!(&ct.0, Value::Variant { payload: None, .. })
    }

    fn dummy_state() -> InferState {
        InferState::new()
    }

    #[test]
    fn from_type_int_produces_typenode_int() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Int, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Int");
        assert!(
            is_unit_variant(&ct),
            "TypeNode.Int must be a unit variant (no payload)"
        );
    }

    #[test]
    fn from_type_int_literal_promotes_to_typenode_int() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::IntLiteral(42), &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Int");
    }

    #[test]
    fn from_type_float_produces_typenode_float() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Float, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Float");
    }

    #[test]
    fn from_type_str_produces_typenode_string() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Str, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.String");
    }

    #[test]
    fn from_type_string_literal_promotes_to_typenode_string() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::StringLiteral("hello".to_string()), &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.String");
    }

    #[test]
    fn from_type_bool_produces_typenode_bool() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Bool, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Bool");
    }

    #[test]
    fn from_type_unknown_produces_typenode_unknown() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Unknown, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Unknown");
        assert!(is_unit_variant(&ct));
    }

    #[test]
    fn from_type_never_produces_typenode_never() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Never, &state);
        assert_eq!(typenode_tag(&ct), "TypeNode.Never");
        assert!(is_unit_variant(&ct));
    }

    /// TypeVar falls back to Unknown until S-862 wires arena access for payload dicts.
    #[test]
    fn from_type_typevar_falls_back_to_unknown_until_s861() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::TypeVar("_t0".to_string(), 1), &state);
        assert_eq!(
            typenode_tag(&ct),
            "TypeNode.Unknown",
            "TypeVar fallback to Unknown expected until S-861 wires arena access"
        );
    }

    /// `fresh_type_var` stub returns Unknown until S-861 wires arena access.
    #[test]
    fn fresh_type_var_stub_returns_unknown_until_s861() {
        let ct = CheckerType::fresh_type_var("_t0".to_string(), 1);
        assert_eq!(
            typenode_tag(&ct),
            "TypeNode.Unknown",
            "fresh_type_var stub returns Unknown until S-861 wires arena access"
        );
    }

    #[test]
    fn checker_type_clone_preserves_tag() {
        let state = dummy_state();
        let ct = CheckerType::from_type(&Type::Int, &state);
        let ct2 = ct.clone();
        assert_eq!(typenode_tag(&ct2), "TypeNode.Int");
    }
}
