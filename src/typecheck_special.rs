//! Special-case type refinement dispatchers for polymorphic builtins.
//!
//! These functions implement domain-specific type checking for builtins that cannot
//! be typed precisely via standard Hindley-Milner type schemes alone. Each function
//! inspects argument types (and sometimes AST structure) to synthesize a precise
//! return type.
//!
//! All functions follow the same pattern:
//!   1. Validate arity
//!   2. Infer argument types (for type-map population)
//!   3. Apply accumulated substitution
//!   4. Dispatch on argument type constructors
//!   5. Return precise result type or `Unknown` gradual fallback
//!
//! Callers live in `infer_surface_expr`'s name-match dispatch block in `typecheck.rs`.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{Span, Spanned, SurfaceExpression, SurfaceNamedArg, SurfaceNode};
use crate::types::{resolve_has_field, unify, InferState, Label, Row, Type, TypeEnv, TypeError};

use super::check_surface_expr;
use super::infer_surface_expr;
use super::TypeMap;

/// Type-check `[get key dict]` and `[get? key dict]` with narrowing on Map/Record argument types.
///
/// For `[get key dict]` (error on missing key):
/// - `Map[K V]` → `V`
/// - `Record` with known string key → field type
/// - Otherwise → `Unknown`
///
/// For `[get? key dict]` (Null on missing key):
/// - `Map[K V]` → `V | Null`
/// - `Record` with known string key → `field_type | Null`
/// - Otherwise → `Unknown`
///
/// "Null" is represented as the empty closed record `Type::Record(Row { fields: {}, tail: Empty })`.
/// Type check `open dir-cap path flag...` — synthesize Handle(cap_row) from flag arguments.
///
/// The `open` builtin accepts a DirCap, a path string, and variadic capability flag arguments:
///   `Readable`, `Writable`, `Appendable`, `Binary`, `Text`, `Seekable`
///
/// These flags are registered in the prelude as `[variant "Name"]` which returns `Unknown`.
/// Static type inspection of the arguments would always see `Unknown` and produce no precision.
/// Instead, this function inspects the **AST** of each flag argument to extract the flag name
/// when the argument is a bare VarRef (the common case: `[open cap path Readable Text]`).
///
/// Synthesized return types:
/// - Flags `Readable`, `Writable`, `Appendable`, `Binary`, `Text`, `Seekable` each contribute
///   a `__cap_flag_<name>` field to the capability row of the returned Handle.
/// - Example: `[open cap path Readable Text]` → `Handle[__cap_flag_readable __cap_flag_text]`
/// - Unknown flags or runtime-computed flag variables → `Handle(Unknown)` (gradual fallback)
///
/// The capability row structure matches the singleton records registered in build_builtins_type_env():
/// each flag name maps to an empty record `Type::Record(Row { fields: {} })` keyed by
/// `"__cap_flag_<name>"`. This matches the `cap_flag(name)` helper in type_env.rs.
///
/// Argument checking:
/// - arg[0]: DirCap — type-checked against `Type::DirCap`
/// - arg[1]: Str (path) — type-checked against `Type::Str`
/// - arg[2..]: flag args — inferred (for type map population); not type-checked against a
///   concrete type because their static type is Unknown (prelude-defined unit variants)
///
/// Runtime validation (at least one of Readable/Writable/Appendable required) is enforced
/// by the builtin at runtime, not statically here. Static arity check: at least 3 args
/// (DirCap + path + 1 flag). This matches the runtime's minimum: `open: requires >= 3 args`.
pub(crate) fn check_open(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require at least 3 args (DirCap, path, at least one flag).
    // Matches builtin_open's runtime check: `if args.len() < 3`.
    if args.len() < 3 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `open` requires at least 3 arguments (DirCap, path, flag...), got {}",
                args.len()
            ),
            span,
        )]);
    }

    let mut errors = Vec::new();

    // Check arg[0]: DirCap
    {
        if let Err(mut errs) = check_surface_expr(&args[0], &Type::DirCap, env, state, type_map) {
            errors.append(&mut errs);
        }
    }

    // Check arg[1]: Str (path)
    {
        if let Err(mut errs) = check_surface_expr(&args[1], &Type::Str, env, state, type_map) {
            errors.append(&mut errs);
        }
    }

    // The set of known open flag names and their canonical cap_row field names.
    // Matches the flags accepted by builtin_open in src/builtins_io.rs and the
    // prelude's [type [Readable] [Writable] [Binary] [Text] [Seekable]] OpenFlag declaration.
    // Appendable is missing from the prelude's Readable re-exports (name conflict with
    // the Appendable typeclass) but IS accepted by the builtin.
    const KNOWN_FLAGS: &[(&str, &str)] = &[
        ("Readable", "readable"),
        ("Writable", "writable"),
        ("Appendable", "appendable"),
        ("Binary", "binary"),
        ("Text", "text"),
        ("Seekable", "seekable"),
    ];

    // Inspect flag arguments (arg[2..]) by AST to extract flag names.
    // We inspect AST rather than inferred types because the prelude registers Readable etc.
    // as `[variant "Readable"]` which types as Unknown — type-level inspection provides no info.
    let mut cap_fields: HashMap<String, Type> = HashMap::new();
    let mut all_flags_known = true;

    for flag_arg in args.iter().skip(2) {
        // Infer the flag arg for type map population (side effect: records hover type for LSP).
        if let Ok(_flag_ty) = infer_surface_expr(flag_arg, env, state, type_map) {
            // Type map already populated by infer_surface_expr above.
        }

        // Inspect AST: if the arg is a VarRef with a known flag name, collect it.
        // Accept both bare `Readable` and escaped `$Readable` forms — both refer to the
        // same prelude-defined variant constructor. The `escaped` field is `true` for `$name`,
        // `false` for bare `name`; both are semantically equivalent in value position.
        let flag_name = match &flag_arg.expr {
            SurfaceExpression::VarRef { name, .. } => KNOWN_FLAGS.iter().find_map(
                |(flag, canonical)| {
                    if name == flag {
                        Some(*canonical)
                    } else {
                        None
                    }
                },
            ),
            _ => None,
        };

        match flag_name {
            Some(canonical) => {
                // Known flag: add to cap row as __cap_flag_<canonical> → empty Record
                cap_fields.insert(
                    format!("__cap_flag_{}", canonical),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                );
            }
            None => {
                // Unknown or runtime-computed flag: cannot determine cap row statically.
                // Fall through to Handle(Unknown) gradual fallback below.
                all_flags_known = false;
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Synthesize the return type:
    // - If all flags are statically known VarRefs: precise Handle(cap_row)
    // - If any flag is a runtime-computed expression: Handle(Unknown) (gradual fallback)
    //
    // Note: even when some flags are known and some are not, we fall back to Handle(Unknown)
    // rather than a partial cap row. A partial row would be misleading — it would claim
    // specific capabilities without being certain that all capabilities are accounted for.
    // The gradual Handle(Unknown) is conservative and correct: it accepts any Handle consumer.
    let cap_type = if all_flags_known && !cap_fields.is_empty() {
        Type::Record(Row { fields: cap_fields })
    } else {
        Type::Unknown
    };

    Ok(Type::Handle(Box::new(cap_type)))
}

/// Type check `connect` — precise return type based on transport variant.
///
/// The static signature in TypeEnv is Union(Handle[readable+writable], DatagramHandle).
/// This special case synthesizes a precise return type based on the transport argument:
/// - Tcp or UnixStream → Handle[Readable, Writable]
/// - Udp or UnixDatagram → DatagramHandle
/// - Unknown transport → Union fallback
pub(crate) fn check_connect(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 4 args (cap, transport, host, port)
    if args.len() != 4 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `connect` requires exactly 4 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer arg types (for type checking, even if we don't use them all)
    for arg in args.iter() {
        infer_surface_expr(arg, env, state, type_map)?;
    }

    // Inspect arg 1 (transport) — check if it's a statically-known VarRef
    let transport_name = if let SurfaceExpression::VarRef { name, .. } = &args[1].expr {
        Some(name.as_str())
    } else {
        None
    };

    // Synthesize return type based on transport
    match transport_name {
        Some("Tcp") | Some("UnixStream") => {
            // Stream transports → Handle[Readable, Writable]
            let cap_fields = HashMap::from([
                (
                    "__cap_flag_readable".to_string(),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ),
                (
                    "__cap_flag_writable".to_string(),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ),
            ]);
            Ok(Type::Handle(Box::new(Type::Record(Row {
                fields: cap_fields,
            }))))
        }
        Some("Udp") | Some("UnixDatagram") => {
            // Datagram transports → DatagramHandle
            Ok(Type::DatagramHandle)
        }
        _ => {
            // Unknown or non-VarRef transport → return union fallback
            let cap_fields = HashMap::from([
                (
                    "__cap_flag_readable".to_string(),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ),
                (
                    "__cap_flag_writable".to_string(),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ),
            ]);
            Ok(Type::normalize_union(vec![
                Type::Handle(Box::new(Type::Record(Row { fields: cap_fields }))),
                Type::DatagramHandle,
            ]))
        }
    }
}

/// Type check `builtin-first` — precise return type based on input collection type.
///
/// The static signature in TypeEnv is Top → Unknown.
/// This special case synthesizes a precise return type:
/// - Seq(T) → T
/// - String → String
/// - Bytes → Int
/// - Unknown or other → Unknown fallback
pub(crate) fn check_first(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 1 arg
    if args.len() != 1 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `builtin-first` requires exactly 1 argument, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer the collection argument's type
    let coll_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let coll_ty = state.subst.apply(&coll_ty);

    // Synthesize return type based on collection type
    match &coll_ty {
        Type::Seq(elem_ty) => Ok((**elem_ty).clone()),
        Type::Str => Ok(Type::Str),
        Type::Bytes => Ok(Type::Int),
        Type::Unknown => Ok(Type::Unknown),
        Type::TypeVar(_, _) => Ok(Type::Unknown), // Unresolved type var → gradual fallback
        _ => Ok(Type::Unknown),                   // Other types (Dict, etc.) → gradual fallback
    }
}

/// Type check `builtin-last` — precise return type based on input collection type.
///
/// The static signature in TypeEnv is Top → Unknown.
/// This special case synthesizes a precise return type:
/// - Seq(T) → T
/// - String → String
/// - Bytes → Int
/// - Unknown or other → Unknown fallback
pub(crate) fn check_last(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 1 arg
    if args.len() != 1 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `builtin-last` requires exactly 1 argument, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer the collection argument's type
    let coll_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let coll_ty = state.subst.apply(&coll_ty);

    // Synthesize return type based on collection type
    match &coll_ty {
        Type::Seq(elem_ty) => Ok((**elem_ty).clone()),
        Type::Str => Ok(Type::Str),
        Type::Bytes => Ok(Type::Int),
        Type::Unknown => Ok(Type::Unknown),
        Type::TypeVar(_, _) => Ok(Type::Unknown), // Unresolved type var → gradual fallback
        _ => Ok(Type::Unknown),                   // Other types (Dict, etc.) → gradual fallback
    }
}

/// Type check `map` — precise return type for Seq input with callback.
///
/// The static signature in TypeEnv is Top → Unknown → Unknown.
/// This special case synthesizes a precise return type for the Seq path:
/// - Seq(A) with callback A → B → Seq(B)
/// - Dict input → Unknown (runtime dispatch, no precise type available)
/// - Unknown or other → Unknown fallback
pub(crate) fn check_map(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 2 args (callback, collection)
    if args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `map` requires exactly 2 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer both argument types
    let callback_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let callback_ty = state.subst.apply(&callback_ty);

    let coll_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let coll_ty = state.subst.apply(&coll_ty);

    // Synthesize return type based on collection and callback
    match (&coll_ty, &callback_ty) {
        (Type::Seq(_elem_ty), Type::Function { ret, .. }) => {
            // Seq(A) with callback → Seq(B) where B is the callback's return type
            Ok(Type::Seq(ret.clone()))
        }
        (Type::Seq(_), _) => {
            // Seq input but callback is not a function (could be Unknown, TypeVar, etc.)
            // Fall back to Unknown
            Ok(Type::Unknown)
        }
        _ => {
            // Dict input or other → Unknown (runtime dispatch)
            Ok(Type::Unknown)
        }
    }
}

/// Type check `builtin-concat` — precise return type for Seq + Seq.
///
/// The static signature in TypeEnv is Appendable a => Appendable b => a → b → Unknown.
/// This special case synthesizes a precise return type for the Seq path:
/// - Seq(T) + Seq(T) → Seq(T)
/// - Dict merge or other → Unknown fallback
pub(crate) fn check_concat(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 2 args
    if args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `builtin-concat` requires exactly 2 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer both argument types
    let arg0_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let arg0_ty = state.subst.apply(&arg0_ty);

    let arg1_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let arg1_ty = state.subst.apply(&arg1_ty);

    // Synthesize return type based on both args
    match (&arg0_ty, &arg1_ty) {
        (Type::Seq(elem_ty0), Type::Seq(elem_ty1)) => {
            // Seq(T) + Seq(T) → Seq(T)
            // Unify the element types to get a common type
            let mut local_subst = state.subst.clone();
            if unify(elem_ty0, elem_ty1, &mut local_subst, state, span).is_ok() {
                let unified_elem = local_subst.apply(elem_ty0);
                state.subst = local_subst; // Commit the unification
                Ok(Type::Seq(Box::new(unified_elem)))
            } else {
                // Element types don't unify → fall back to Unknown
                Ok(Type::Unknown)
            }
        }
        _ => {
            // Dict merge or other types → Unknown
            Ok(Type::Unknown)
        }
    }
}

/// Type check `tls-layer` — preserve input handle's capability row.
///
/// The static signature in TypeEnv is Handle(Unknown) → ... → Handle(Unknown).
/// This special case preserves the input handle's capability row:
/// - Handle[α] → ... → Handle[α] (same capabilities)
/// - Unknown → Handle(Unknown) fallback
pub(crate) fn check_tls_layer(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity check: require exactly 3 args (handle, hostname, opts)
    if args.len() != 3 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `tls-layer` requires exactly 3 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer all argument types (for type checking)
    let handle_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let handle_ty = state.subst.apply(&handle_ty);

    // Infer the other args to check them, but we don't use their types
    infer_surface_expr(&args[1], env, state, type_map)?; // hostname
    infer_surface_expr(&args[2], env, state, type_map)?; // opts

    // Preserve the handle's capability row
    match &handle_ty {
        Type::Handle(cap_row) => {
            // Return Handle with the same capability row
            Ok(Type::Handle(cap_row.clone()))
        }
        Type::Unknown => {
            // Unknown handle → fall back to Handle(Unknown)
            Ok(Type::Handle(Box::new(Type::Unknown)))
        }
        _ => {
            // Non-handle argument → fall back to Handle(Unknown)
            // (This should ideally be a type error, but we're being conservative)
            Ok(Type::Handle(Box::new(Type::Unknown)))
        }
    }
}

/// Type check `get-in` — chained field access.
/// [GET-IN-NIL]: empty path returns dict unchanged
/// [GET-IN-CONS]: unfold via repeated field access
pub(crate) fn check_get_in(
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::ast::SurfaceExpression as SE;

    // Validate arity: exactly 2 positional args, no named args
    if !named_args.is_empty() || args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: get-in expects exactly 2 arguments, got {} ({} named)",
                args.len(),
                named_args.len()
            ),
            span,
        )]);
    }

    // Infer the dict type
    let dict_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let dict_ty = state.subst.apply(&dict_ty);

    // Check if path is a literal dict with auto-indexed string entries
    let path_expr = &args[0].expr;
    match path_expr {
        SE::Dict(entries) => {
            // Check all entries are auto-indexed StringLiterals
            let mut keys = Vec::new();
            for (idx, entry) in entries.iter().enumerate() {
                // Check if auto-indexed (key is None or matches index)
                let is_auto_indexed = match &entry.node.key {
                    None => true,
                    Some(key_expr) => {
                        matches!(&key_expr.expr, SE::Int(n) if *n == idx as i64)
                    }
                };

                if !is_auto_indexed {
                    // Gradual: non-auto-indexed entry in path — fall back to Unknown
                    return Ok(Type::Unknown);
                }

                // Check if value is a string literal
                match &entry.node.value.expr {
                    SE::Str(s) => keys.push(s.clone()),
                    _ => {
                        // Gradual: non-literal path element — fall back to Unknown
                        return Ok(Type::Unknown);
                    }
                }
            }

            // Empty path: return dict type unchanged ([GET-IN-NIL])
            if keys.is_empty() {
                return Ok(dict_ty);
            }

            // Unfold via repeated field access ([GET-IN-CONS])
            let mut current_ty = dict_ty;
            for key in keys {
                // Apply substitution before pattern matching to dereference bound TypeVars
                current_ty = state.subst.apply(&current_ty);

                match &current_ty {
                    Type::Record(row) => {
                        if let Some(field_ty) = row.fields.get(&key) {
                            current_ty = field_ty.clone();
                        } else {
                            // Gradual: field not found in get-in path
                            return Ok(Type::Unknown);
                        }
                    }
                    Type::Union(_) | Type::Intersection(_) | Type::Top => {
                        // Resolve via resolve_has_field
                        match resolve_has_field(
                            &Label::Concrete(key),
                            &current_ty,
                            state,
                            span.clone(),
                            0,
                        ) {
                            Ok(field_ty) => current_ty = field_ty,
                            // Gradual: resolve_has_field failed in get-in path
                            Err(_) => return Ok(Type::Unknown),
                        }
                    }
                    // Gradual: Unknown propagates through get-in chain
                    Type::Unknown => return Ok(Type::Unknown),
                    _ => {
                        // Gradual: not a record or union in get-in path
                        return Ok(Type::Unknown);
                    }
                }
            }

            Ok(current_ty)
        }
        _ => {
            // Gradual: path is not a literal sequence
            Ok(Type::Unknown)
        }
    }
}

/// Type check `get` / `builtin-get` — precise return type via Indexable-like dispatch.
///
/// The prelude `get` is annotated `[fn@[return: a] [k@Label xs] ...]` which loses the
/// `Indexable c k v` functional dependency precision when the prelude scheme is instantiated.
/// This special case restores precision for the common collection types:
///
/// - `Seq(T)` → `T`  (integer index into a sequence)
/// - `Map(K, V)` → `V`  (any key into a map)
/// - `Record(row)` + `StringLiteral(field)` → `row[field]`  (known field access)
/// - `Record(row)` + non-literal key → `Unknown`  (gradual fallback)
/// - `Unknown`, `TypeVar`, `Top`, `Error` → `Unknown`  (gradual fallback)
pub(crate) fn check_get(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Arity: exactly 2 positional args (key, collection)
    if args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `get` requires exactly 2 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    // Infer key type
    let key_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let key_ty = state.subst.apply(&key_ty);

    // Infer collection type
    let coll_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let coll_ty = state.subst.apply(&coll_ty);

    match &coll_ty {
        // Seq(T): integer index → element type T
        Type::Seq(elem_ty) => Ok((**elem_ty).clone()),

        // Map(K, V): any key → value type V
        Type::Map(_, val_ty) => Ok((**val_ty).clone()),

        // Record: field access — only precise when key is a StringLiteral
        Type::Record(row) => {
            match &key_ty {
                Type::StringLiteral(field) => {
                    if let Some(field_ty) = row.fields.get(field.as_str()) {
                        Ok(field_ty.clone())
                    } else {
                        // Field not present in closed row — gradual fallback
                        Ok(Type::Unknown)
                    }
                }
                // Non-literal string key or integer key into record — gradual fallback
                _ => Ok(Type::Unknown),
            }
        }

        // Union: try to extract a common field type across all union members
        Type::Union(members) => {
            if let Type::StringLiteral(field) = &key_ty {
                let field = field.clone();
                let mut field_types = Vec::new();
                for member in members {
                    if let Type::Record(row) = member {
                        if let Some(ft) = row.fields.get(field.as_str()) {
                            field_types.push(ft.clone());
                        } else {
                            // One member is missing the field — unknown
                            return Ok(Type::Unknown);
                        }
                    } else {
                        return Ok(Type::Unknown);
                    }
                }
                if field_types.len() == 1 {
                    Ok(field_types.remove(0))
                } else if field_types.is_empty() {
                    Ok(Type::Unknown)
                } else {
                    Ok(Type::Union(field_types))
                }
            } else {
                Ok(Type::Unknown)
            }
        }

        // Gradual fallback for Unknown/TypeVar/Top/Error
        Type::Unknown | Type::TypeVar(_, _) | Type::Top | Type::Error => Ok(Type::Unknown),

        // Any other type — gradual fallback (not a known collection type)
        _ => Ok(Type::Unknown),
    }
}

/// Type check `+` / `-` / `*` and their `builtin-add`/`builtin-sub`/`builtin-mul` aliases.
///
/// The prelude wraps these operators with a scheme that returns `Number` for all inputs,
/// which loses precision when both operands are `Int`. This special case refines the
/// return type based on the operand types:
///
/// - `Int op Int` → `Int`
/// - `Float op _` or `_ op Float` → `Float`
/// - `Number op Number` → `Number`  (no further refinement possible)
/// - `TypeVar`, `Unknown`, `Top` → pass through (may resolve to numeric at runtime)
/// - `Str`, `Bool`, `Record`, etc. → `TypeError` (provably non-numeric)
pub(crate) fn check_arithmetic(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    if args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: arithmetic operator requires exactly 2 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    let lhs_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let lhs_ty = state.subst.apply(&lhs_ty);

    let rhs_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let rhs_ty = state.subst.apply(&rhs_ty);

    // Validate operands are not provably non-numeric.
    // Conservative: StringLiteral is NOT flagged as definitely non-numeric here.
    // Adding StringLiteral would cause [+ 1 "str"] to produce a static type error,
    // which breaks the failed_bindings_error corpus test (expects arithmetic to fail at
    // runtime only: x gets Type::Error from constraint check, downstream gets "undefined var").
    fn is_definitely_non_numeric(ty: &Type) -> bool {
        !matches!(
            ty,
            Type::Unknown
                | Type::TypeVar(_, _)
                | Type::Top
                | Type::Error
                | Type::Never
                | Type::Number
                | Type::Int
                | Type::Float
                | Type::IntLiteral(_)
                | Type::StringLiteral(_)
        )
    }

    if is_definitely_non_numeric(&lhs_ty) {
        return Err(vec![TypeError::new(
            format!("arithmetic operator applied to non-numeric type `{lhs_ty}`"),
            span,
        )]);
    }
    if is_definitely_non_numeric(&rhs_ty) {
        return Err(vec![TypeError::new(
            format!("arithmetic operator applied to non-numeric type `{rhs_ty}`"),
            span,
        )]);
    }

    // Refine return type: Float beats Int beats Number
    let result_ty = match (&lhs_ty, &rhs_ty) {
        // Any Float operand → Float
        (Type::Float, _) | (_, Type::Float) => Type::Float,
        // Both Int or IntLiteral → Int
        (Type::Int | Type::IntLiteral(_), Type::Int | Type::IntLiteral(_)) => Type::Int,
        // Otherwise (Number, TypeVar, Unknown mix) → Number
        _ => Type::Number,
    };

    Ok(result_ty)
}

/// Type check `/` / `builtin-div` — always returns `Float` (IEEE division semantics).
///
/// Integer division in LLT always produces a Float (unlike integer-truncating `/` in some
/// languages). This matches the runtime behavior of `builtin-div`.
pub(crate) fn check_div(
    args: &[Arc<SurfaceNode>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    if args.len() != 2 {
        return Err(vec![TypeError::new(
            format!(
                "arity mismatch: `/` requires exactly 2 arguments, got {}",
                args.len()
            ),
            span,
        )]);
    }

    let lhs_ty = infer_surface_expr(&args[0], env, state, type_map)?;
    let lhs_ty = state.subst.apply(&lhs_ty);

    let rhs_ty = infer_surface_expr(&args[1], env, state, type_map)?;
    let rhs_ty = state.subst.apply(&rhs_ty);

    // Validate operands are not provably non-numeric (same conservative policy as check_arithmetic).
    fn is_definitely_non_numeric(ty: &Type) -> bool {
        !matches!(
            ty,
            Type::Unknown
                | Type::TypeVar(_, _)
                | Type::Top
                | Type::Error
                | Type::Never
                | Type::Number
                | Type::Int
                | Type::Float
                | Type::IntLiteral(_)
                | Type::StringLiteral(_)
        )
    }

    if is_definitely_non_numeric(&lhs_ty) {
        return Err(vec![TypeError::new(
            format!("division operator applied to non-numeric type `{lhs_ty}`"),
            span,
        )]);
    }
    if is_definitely_non_numeric(&rhs_ty) {
        return Err(vec![TypeError::new(
            format!("division operator applied to non-numeric type `{rhs_ty}`"),
            span,
        )]);
    }

    // Division always yields Float (IEEE semantics)
    Ok(Type::Float)
}

/// Type check an inferred `[do]` form — the do-infer sentinel (e.g., `ℊꜱʏᴍ⧼do-infer⧽0.bind`).
///
/// The `do` macro emits `[ℊꜱʏᴍ⧼do-infer⧽N.bind e [fn [x] ...]]` when no explicit monad is provided.
/// This function:
///   1. Resolves the monad variable name (Rule 1: from `state.expected_return`, Rule 2: from
///      the first arg's inferred type, AST fallback: from syntactic constructor pattern,
///      Rule 3: emit TypeError).
///   2. Records `sentinel_name → monad_name` in `state.do_infer_resolutions` so the evaluator
///      can substitute the sentinel with the concrete monad dict at runtime.
///   3. Infers all argument expressions for type-map population and side effects.
///   4. Returns the expected return type (if available) or a fresh TypeVar.
///
/// **Monad resolution heuristics** (simplified — full HKT inference requires `App(m, a)` types):
///   - Rule 1 (type-level): If `state.expected_return` is a Record with `ok`/`err` fields, or
///     a union of such records, resolve to the `"result"` monad dict.
///   - Rule 2 (type-level): If the first arg's inferred type matches `App(m, _)` where m has a
///     registered Monad instance, or is a Result-like Record, resolve to the corresponding monad dict.
///   - AST fallback (syntactic): If type-level resolution fails, inspect the first arg's AST.
///     If it's a call to a nominal constructor (`[Ok ...]`, `[Error ...]`), resolve to the
///     corresponding monad dict.
///   - Rule 3 (failure): If all resolution attempts fail, emit TypeError T_DO_INFER.
#[allow(clippy::too_many_arguments)] // Signature matches check_call pattern
pub(crate) fn check_do_infer(
    method: &crate::ast::DotKey,
    sentinel_name: &str,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Rc<TypeEnv>,
    call_span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let method_str = match method {
        crate::ast::DotKey::Ident(s) => s.as_str(),
        crate::ast::DotKey::Int(n) => {
            return Err(vec![TypeError::new(
                format!("inferred [do]: unexpected integer method index {n} on {sentinel_name}"),
                call_span,
            )]);
        }
    };

    // Step 1: Resolve the monad name from context.
    // Check if we've already resolved this sentinel (fast path).
    if let Some(_existing) = state.do_infer_resolutions.get(sentinel_name) {
        // Already resolved — infer remaining args for side effects and return the expected type.
        for arg in args {
            let _ = infer_surface_expr(arg, env, state, type_map);
        }
        for na in named_args {
            let _ = infer_surface_expr(&na.node.value, env, state, type_map);
        }
        let ret = match method_str {
            "bind" | "pure" => {
                if let Some(ret_ty) = state.expected_return.clone() {
                    state.subst.apply(&ret_ty)
                } else {
                    // Gradual: no expected_return context — bind? return type unknown
                    Type::Unknown
                }
            }
            // Gradual: non-Result monad type — bind? return type unknown
            _ => Type::Unknown,
        };
        return Ok(ret);
    }

    // Rule 1: Check state.expected_return for a Result-like type.
    let resolved = if let Some(ret_ty) = state.expected_return.clone() {
        let applied = state.subst.apply(&ret_ty);
        resolve_monad_from_type(&applied, state)
    } else {
        None
    };

    // Rule 2: If Rule 1 failed, infer the first arg's type (for side effects too),
    // then check if it resolves to a known monad.
    // first_arg_already_inferred tracks whether we consumed the first arg here,
    // so Step 3 can skip it to avoid double-inference.
    let (resolved, first_arg_already_inferred) = if resolved.is_none() && !args.is_empty() {
        let first_arg_ty = infer_surface_expr(&args[0], env, state, type_map)
            .ok()
            .map(|ty| state.subst.apply(&ty));
        let rule2_result = first_arg_ty.and_then(|ty| resolve_monad_from_type(&ty, state));
        (rule2_result, true)
    } else {
        (resolved, false)
    };

    // Rule 2b — AST fallback: If type-level resolution failed, try syntactic pattern matching.
    // This handles nominal constructors like [Ok ...] and [Error ...] whose types
    // currently infer as Unknown.
    let resolved = if resolved.is_none() && !args.is_empty() {
        resolve_monad_from_surface(&args[0])
    } else {
        resolved
    };

    // Rule 3: If no rule worked, emit TypeError.
    let (monad_name, first_arg_already_inferred) = match resolved {
        Some(name) => (name, first_arg_already_inferred),
        None => {
            // Infer remaining args for type map population before returning error.
            let start = if first_arg_already_inferred { 1 } else { 0 };
            for arg in args.iter().skip(start) {
                let _ = infer_surface_expr(arg, env, state, type_map);
            }
            for na in named_args {
                let _ = infer_surface_expr(&na.node.value, env, state, type_map);
            }
            return Err(vec![TypeError::new(
                "cannot infer monad for [do] — add an explicit monad argument (e.g., [do result ...])",
                call_span,
            )
            .with_code("T_DO_INFER")]);
        }
    };

    // Step 2: Record sentinel_name → monad_name for eval wiring.
    state
        .do_infer_resolutions
        .insert(sentinel_name.to_string(), monad_name.clone());

    // Step 3: Infer all remaining args for type-map population and side effects.
    // Skip the first arg if Rule 2 already inferred it (avoid double-inference side effects).
    let start = if first_arg_already_inferred { 1 } else { 0 };
    for arg in args.iter().skip(start) {
        let _ = infer_surface_expr(arg, env, state, type_map);
    }
    for na in named_args {
        let _ = infer_surface_expr(&na.node.value, env, state, type_map);
    }

    // Step 4: Return the expected return type or a fresh TypeVar.
    // For "bind": the return type is the monad applied to the continuation's return type.
    // Without precise bind types, return expected_return (if set) or a fresh TypeVar.
    let ret = match method_str {
        "bind" | "pure" => {
            if let Some(ret_ty) = state.expected_return.clone() {
                state.subst.apply(&ret_ty)
            } else {
                state.fresh_type_var()
            }
        }
        _ => state.fresh_type_var(),
    };

    Ok(ret)
}

/// Heuristic: resolve a monad dict variable name from a type.
///
/// Type-level resolution rules:
///   - Record with `ok` and/or `err` fields → "result"
///   - Union containing records with ok/err fields → "result"
///   - `App(Operator("Result"), _)` → "result"
///   - `Operator("Result")` (bare type constructor) → "result"
///   - `Seq(_)` → would be "seq-monad" (not yet implemented)
///
/// Returns `Some(monad_var_name)` if a known monad is recognized, `None` otherwise.
///
/// Note: If type-level resolution fails, see `resolve_monad_from_expr` for AST-level fallback.
pub(crate) fn resolve_monad_from_type(ty: &Type, _state: &InferState) -> Option<String> {
    match ty {
        // App(Result, _) — nominal Result type constructor applied to a type argument
        Type::App(f, _) => {
            if let Type::Operator(name) = f.as_ref() {
                if name == "Result" {
                    return Some("result".to_string());
                }
            }
            None
        }
        // Operator("Result") — bare Result type constructor (not yet applied to a type arg).
        //
        // Reachability: this arm is reached when the inferred type of a [do] binding's RHS
        // is the bare type constructor `Result` rather than `App(Result, a)`. In the current
        // type system, this can occur if a variable is annotated as `@Result` (the operator
        // itself, without a type argument) or if a future typed-expr-constructors pass emits
        // Operator("Result") before application. With the current untyped variant constructors
        // (Ok/Error infer as Unknown), Rule 2 type-level never reaches this arm in practice —
        // the AST fallback (Rule 2b / resolve_monad_from_expr) handles those cases instead.
        //
        // TODO: verify reachability once constructor types are tracked (typed-expr-constructors
        // sprint). If App(Result, _) always subsumes bare Operator("Result") after that sprint,
        // this arm can be removed.
        Type::Operator(name) => {
            if name == "Result" {
                Some("result".to_string())
            } else {
                None
            }
        }
        // Record with ok and/or err fields — structural Result-like type
        Type::Record(row) => {
            if row.fields.contains_key("ok") || row.fields.contains_key("err") {
                Some("result".to_string())
            } else {
                None
            }
        }
        // Union — check if all members that resolve to a monad agree on the same one
        Type::Union(members) => {
            let mut resolved = None;
            for m in members {
                if let Some(name) = resolve_monad_from_type(m, _state) {
                    if let Some(ref prev) = resolved {
                        if prev != &name {
                            return None; // disagreement
                        }
                    } else {
                        resolved = Some(name);
                    }
                }
            }
            resolved
        }
        // Seq — could be seq monad, but no dict var exists yet
        // Type::Seq(_) => Some("seq-monad".to_string()),
        _ => None,
    }
}

/// AST-level fallback for monad resolution when type inference fails.
///
/// Syntactic resolution rules:
///   - `Call { func: VarRef("Ok"), implied: true, .. }` → "result"
///   - `Call { func: VarRef("Error"), implied: true, .. }` → "result"
///
/// This is a FALLBACK — `resolve_monad_from_type` takes priority. Only used when
/// type-level inference returns `Unknown` or another non-resolvable type.
///
/// Returns `Some(monad_var_name)` if a known constructor pattern is recognized, `None` otherwise.
pub(crate) fn resolve_monad_from_surface(node: &Arc<SurfaceNode>) -> Option<String> {
    match &node.expr {
        SurfaceExpression::Call {
            func,
            implied: true,
            ..
        } => {
            // Check if func is a VarRef to Ok or Error
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "Ok" || name == "Error" {
                    return Some("result".to_string());
                }
            }
            None
        }
        _ => None,
    }
}
