//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Span, Spanned};
use crate::types::{
    generalize, instantiate_at_level, instantiate_scheme, lower_row_var_levels_pub,
    row_var_occurs_pub, unify, InferState, Row, RowTail, Substitution, Type, TypeEnv, TypeError,
    TypeScheme,
};

/// Map from source span `(start_offset, end_offset)` to inferred type. Populated during type
/// checking so LSP hover/diagnostics can look up types without re-running inference. Offsets
/// are sufficient as keys; the full `Span` source text is not needed.
pub type TypeMap = HashMap<(usize, usize), Type>;

/// Type-check a parsed [`File`].
///
/// Returns `Ok(())` if no type errors are found, or `Err(errors)` with the list of
/// [`TypeError`]s. Type checking is advisory — the evaluator proceeds regardless.
///
/// # Precondition
///
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// Without the desugar pass, `$_` expressions appear as bare `VarRef("_")` nodes,
/// producing spurious `"undefined variable _"` type errors. All pipeline entry points
/// already call `desugar_file` first; see `eval_source_with_config` in `lib.rs` for
/// the canonical call sequence.
pub fn typecheck_file(file: &File) -> Result<(), Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::with_builtins());
    let mut state = InferState::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: RowTail::Empty,
    });

    for doc in &file.documents {
        match typecheck_document(
            doc,
            &env,
            &mut state,
            &mut None,
            &pipeline_type,
            &named_types,
        ) {
            Ok((new_env, doc_output_type, mut advisory)) => {
                env = new_env;
                // Report advisory errors (expects:/output_type) without blocking propagation.
                errors.append(&mut advisory);
                // Store named section type if this document has a name
                if let Some(ref name) = doc.node.name {
                    named_types.insert(name.clone(), doc_output_type.clone());
                }
                // Update pipeline type for next document
                pipeline_type = doc_output_type;
            }
            Err(mut doc_errors) => errors.append(&mut doc_errors),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Reset all elaboration state in the AST (TypeAssert.resolved_type fields).
/// This allows re-typechecking a cached AST without triggering the write-once
/// invariant assertion in resolve_type_assert.
///
/// Uses interior mutability via RefCell, so only needs &File (not &mut File).
fn reset_elaboration(file: &File) {
    for doc in &file.documents {
        for expr in &doc.node.expressions {
            reset_expr(expr.as_ref());
        }
    }
}

/// Recursively reset resolved_type in all TypeAssert nodes.
fn reset_expr(expr: &Spanned<Expr>) {
    match &expr.node {
        // Literals: no children
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef(_)
        | Expr::Rest(_)
        | Expr::Error(_) => {}

        // Access expressions: recurse into target and key/bounds
        Expr::DotAccess { expr: target, .. } => {
            reset_expr(target);
        }
        Expr::BracketAccess {
            expr: target,
            key: key_expr,
        } => {
            reset_expr(target);
            reset_expr(key_expr);
        }
        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => {
            reset_expr(target);
            if let Some(s) = start {
                reset_expr(s);
            }
            if let Some(e) = end {
                reset_expr(e);
            }
        }

        // Dict: recurse into keys and values
        Expr::Dict(entries) => {
            for entry_spanned in entries {
                if let Some(key_spanned) = &entry_spanned.node.key {
                    reset_expr(key_spanned);
                }
                reset_expr(&entry_spanned.node.value);
            }
        }

        // Call: recurse into func, args, and named args
        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => {
            reset_expr(func);
            for arg in args {
                reset_expr(arg);
            }
            for named_arg_spanned in named_args {
                reset_expr(&named_arg_spanned.node.value);
            }
        }

        // Fn: recurse into parameter annotations, return annotation, and body
        Expr::Fn {
            params: _,
            body,
            return_ann: _,
            desugared: _,
        } => {
            // Annotations (param types, return type) are type expressions and
            // cannot contain TypeAssert nodes — no reset needed.
            reset_expr(body);
        }

        // TypeAlias: recurse into the aliased expression
        Expr::TypeAlias(inner) => {
            reset_expr(inner);
        }

        // TypeAssert: reset resolved_type and recurse into inner expression
        // Note: we don't recurse into the annotation because annotations are
        // type expressions and shouldn't contain TypeAssert nodes
        Expr::TypeAssert {
            expr: inner,
            resolved_type,
            ..
        } => {
            *resolved_type.borrow_mut() = None;
            reset_expr(inner);
        }

        // Annotated: annotations don't contain TypeAssert nodes, so nothing to reset
        Expr::Annotated { .. } => {}
    }
}

/// Type-check a file, returning both errors and a map from expression spans to
/// inferred types. The type map is populated even when errors occur, covering
/// every expression that was successfully inferred.
///
/// Automatically resets elaboration state before typechecking, allowing
/// re-typechecking of cached ASTs (LSP use case) without triggering the
/// write-once invariant assertion in resolve_type_assert.
///
/// # Precondition
///
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// See [`typecheck_file`] for details.
pub fn typecheck_file_with_types(file: &File) -> (Vec<TypeError>, TypeMap) {
    // Reset elaboration state to allow re-typechecking cached ASTs
    reset_elaboration(file);

    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::with_builtins());
    let mut state = InferState::new();
    let mut type_map = TypeMap::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: RowTail::Empty,
    });

    for doc in &file.documents {
        match typecheck_document(
            doc,
            &env,
            &mut state,
            &mut Some(&mut type_map),
            &pipeline_type,
            &named_types,
        ) {
            Ok((new_env, doc_output_type, mut advisory)) => {
                env = new_env;
                // Report advisory errors (expects:/output_type) without blocking propagation.
                errors.append(&mut advisory);
                // Store named section type if this document has a name
                if let Some(ref name) = doc.node.name {
                    named_types.insert(name.clone(), doc_output_type.clone());
                }
                // Update pipeline type for next document
                pipeline_type = doc_output_type;
            }
            Err(mut doc_errors) => errors.append(&mut doc_errors),
        }
    }

    (errors, type_map)
}

/// Helper for tests that don't need pipeline/named types.
/// Advisory errors (expects:/output_type) are promoted to fatal in this helper so that
/// test assertions written as `.expect("typecheck should succeed")` still catch them.
#[cfg(test)]
fn typecheck_document_simple(
    doc: &Spanned<Document>,
    parent_env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Rc<TypeEnv>, Vec<TypeError>> {
    let empty_record = Type::Record(Row {
        fields: HashMap::new(),
        tail: RowTail::Empty,
    });
    let named_types = HashMap::new();
    typecheck_document(
        doc,
        parent_env,
        state,
        type_map,
        &empty_record,
        &named_types,
    )
    .and_then(|(env, _ty, advisory)| {
        if advisory.is_empty() {
            Ok(env)
        } else {
            Err(advisory)
        }
    })
}

/// `expects:` and `output_type` annotation errors are advisory — they are returned inside the
/// `Ok` tuple so callers can propagate pipeline types even when an annotation check fails.
/// Fatal body errors (inference failures, undefined variables, etc.) are returned as `Err`.
fn typecheck_document(
    doc: &Spanned<Document>,
    parent_env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    pipeline_type: &Type,
    named_types: &HashMap<String, Type>,
) -> Result<(Rc<TypeEnv>, Type, Vec<TypeError>), Vec<TypeError>> {
    let mut errors = Vec::new();
    // Advisory errors from `expects:` and `output_type` annotations. These do not block
    // pipeline-type propagation — they are returned in the Ok tuple so callers can continue
    // threading types across --- boundaries even when an annotation contract is violated.
    let mut advisory_errors: Vec<TypeError> = Vec::new();

    // Create environment with % and %name bindings
    let mut env = TypeEnv::with_parent(parent_env);

    // Bind % (pipeline variable) with the incoming type
    env.insert("%".to_string(), pipeline_type.clone());

    // Bind all named sections as %name
    for (name, ty) in named_types {
        env.insert(format!("%{}", name), ty.clone());
    }

    let mut env = Rc::new(env);

    // Validate expects annotation if present.
    // `expects:` is advisory: errors go into advisory_errors, not errors, so a contract
    // violation does not block pipeline-type propagation for subsequent documents.
    if let Some(ref expects_ann) = doc.node.expects {
        match resolve_annotation(
            &expects_ann.node,
            &env,
            expects_ann.span,
            state,
            &mut None,
            &mut None,
        ) {
            Ok(expected_type) => {
                if !Type::is_subtype(pipeline_type, &expected_type) {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Pipeline input type {} does not satisfy expects contract {}",
                            pipeline_type, expected_type
                        ),
                        expects_ann.span,
                    ));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    let mut result_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: RowTail::Empty,
    });

    let exprs = &doc.node.expressions;
    if exprs.is_empty() {
        // Validate output annotation even for empty document.
        // output_type annotation mismatches are advisory; they do not block Ok.
        if let Some(ref output_ann) = doc.node.output_type {
            match resolve_annotation(
                &output_ann.node,
                &env,
                output_ann.span,
                state,
                &mut None,
                &mut None,
            ) {
                Ok(expected_output) => {
                    if !Type::is_subtype(&result_type, &expected_output) {
                        advisory_errors.push(TypeError::new(
                            format!(
                                "Document output type {} does not match annotation {}",
                                result_type, expected_output
                            ),
                            output_ann.span,
                        ));
                    }
                }
                Err(e) => advisory_errors.push(e),
            }
        }

        let mut result_env = TypeEnv::with_parent(&env);
        result_env.insert("%".to_string(), result_type.clone());

        // Body is empty (no fatal errors possible), always return Ok.
        // advisory_errors (expects:/output_type) are included in Ok for the caller to report.
        if errors.is_empty() {
            return Ok((Rc::new(result_env), result_type, advisory_errors));
        } else {
            return Err(errors);
        }
    }

    let mut last_dict_schemes: Option<HashMap<String, TypeScheme>> = None;
    // Carries the inferred Record type and the enclosing_level saved before inference,
    // so that generalization in the block below the loop uses the correct level explicitly.
    let mut last_record_type: Option<(Type, u32)> = None;
    let mut last_expr: Option<&Spanned<Expr>> = None;

    for (i, expr_rc) in exprs.iter().enumerate() {
        let expr = expr_rc.as_ref();
        let is_last = i == exprs.len() - 1;

        // Special handling for Dict expressions at document level to preserve schemes
        if let Expr::Dict(entries) = &expr.node {
            match infer_dict(entries, &env, state, type_map, expr.span) {
                Ok((ty, schemes)) => {
                    if is_last {
                        result_type = ty;
                        last_dict_schemes = Some(schemes);
                        last_expr = Some(expr);
                    } else {
                        // Record the inferred dict type in type_map so LSP hover works
                        // for non-last Dict positions in a document. infer_dict is called
                        // directly here (bypassing infer_expr), so type_map insertion
                        // must be done explicitly — infer_expr's auto-insert at line 522
                        // is not reached for this code path.
                        if let Some(ref mut map) = type_map {
                            let key = (expr.span.start.offset, expr.span.end.offset);
                            map.insert(key, ty.clone());
                        }
                        let mut new_env = TypeEnv::with_parent(&env);
                        // Thread schemes into the environment
                        for (name, scheme) in &schemes {
                            new_env.insert_scheme(name.clone(), scheme.clone());
                        }
                        let mut alias_errs = register_type_aliases(expr, &mut new_env, &env, state);
                        errors.append(&mut alias_errs);
                        env = Rc::new(new_env);
                    }
                }
                Err(mut errs) => errors.append(&mut errs),
            }
        } else {
            if is_last {
                // Last expression: always infer at an incremented level so that any type
                // variables introduced during inference are at a higher level than the
                // document boundary, making them generalizable when threading field schemes.
                // The level is restored immediately after inference (line 127).
                let enclosing_level = state.level;
                state.level += 1;

                match infer_expr(expr, &env, state, type_map) {
                    Ok(ty) => {
                        state.level = enclosing_level;
                        result_type = ty.clone();
                        // Track last non-Dict Record type and its enclosing_level for scheme
                        // threading. Storing enclosing_level here makes it available explicitly
                        // at the generalization site below (defense-in-depth per Kiselyov 2013).
                        if matches!(&ty, Type::Record(_)) {
                            last_record_type = Some((ty, enclosing_level));
                        }
                        last_expr = Some(expr);
                    }
                    Err(mut errs) => {
                        state.level = enclosing_level;
                        errors.append(&mut errs);
                        // Populate type_map with Error for LSP hover on failed expressions.
                        // infer_expr already inserts Type::Error into type_map before returning Err,
                        // but typecheck_document re-inserts here for the outer span (the document-level
                        // expression span may differ from the inner sub-expression span that infer_expr
                        // recorded). Use Type::Error (not Any) so LSP shows <error> not Any.
                        if let Some(ref mut map) = type_map {
                            let key = (expr.span.start.offset, expr.span.end.offset);
                            map.insert(key, Type::Error);
                        }
                    }
                }
            } else {
                // Non-last expression: infer at incremented level (mirroring Dict's level
                // management) so that type variables can be properly generalized when
                // threading Record fields as schemes into the environment.
                let enclosing_level = state.level;
                state.level += 1;

                match infer_expr(expr, &env, state, type_map) {
                    Ok(ty) => {
                        state.level = enclosing_level; // Restore before generalization
                        match &ty {
                            Type::Record(Row { fields, .. }) => {
                                // Non-dict Record expressions (e.g., from a function call)
                                // thread field types as generalized schemes, mirroring Dict behavior.
                                // This enables polymorphic field access for Records returned from
                                // polymorphic functions.
                                let mut new_env = TypeEnv::with_parent(&env);
                                for (name, field_ty) in fields {
                                    let scheme = generalize(enclosing_level, field_ty, state);
                                    new_env.insert_scheme(name.clone(), scheme);
                                }
                                let mut alias_errs =
                                    register_type_aliases(expr, &mut new_env, &env, state);
                                errors.append(&mut alias_errs);
                                env = Rc::new(new_env);
                            }
                            Type::Any => {}
                            _ => errors.push(TypeError::not_a_record(&ty, expr.span)),
                        }
                    }
                    Err(mut errs) => {
                        state.level = enclosing_level; // Restore even on error
                        errors.append(&mut errs);
                        // Populate type_map with Error for LSP hover on failed expressions.
                        // Use Type::Error (not Any) so LSP shows <error> not Any (see comment
                        // in the last-expression error path above).
                        if let Some(ref mut map) = type_map {
                            let key = (expr.span.start.offset, expr.span.end.offset);
                            map.insert(key, Type::Error);
                        }
                    }
                }
            }
        }
    }

    let mut result_env = TypeEnv::with_parent(&env);

    // If the last expression was a dict, thread its schemes into the result environment
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env.insert_scheme(name, scheme);
        }
    }

    // If the last expression was a non-Dict Record, generalize and thread its fields.
    // enclosing_level is the level that was active before inference of the last expression.
    // At this point state.level has been restored to enclosing_level (line 127 above), so
    // `enclosing_level == state.level`, but we use the named variable stored in
    // last_record_type for explicitness and defense-in-depth (Kiselyov 2013): any type
    // variable with ℓ(α) > enclosing_level is generalizable, exactly mirroring infer_dict
    // Pass 4 which generalizes at the enclosing level it saved before incrementing.
    if let Some((Type::Record(Row { fields, .. }), enclosing_level)) = last_record_type {
        for (name, field_ty) in fields {
            let scheme = generalize(enclosing_level, &field_ty, state);
            result_env.insert_scheme(name, scheme);
        }
    }

    // Register type aliases from the last expression
    // Note: errors are intentionally ignored here, matching the behavior of infer_dict Pass 2.
    // Type alias resolution errors are reported when the aliases are used, not when registered.
    if let Some(expr) = last_expr {
        let _ = register_type_aliases(expr, &mut result_env, &env, state);
    }

    // Validate output annotation if present.
    // output_type annotation mismatches are advisory; they do not block Ok.
    if let Some(ref output_ann) = doc.node.output_type {
        match resolve_annotation(
            &output_ann.node,
            &result_env,
            output_ann.span,
            state,
            &mut None,
            &mut None,
        ) {
            Ok(expected_output) => {
                if !Type::is_subtype(&result_type, &expected_output) {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Document output type {} does not match annotation {}",
                            result_type, expected_output
                        ),
                        output_ann.span,
                    ));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    result_env.insert("%".to_string(), result_type.clone());

    if errors.is_empty() {
        // No fatal body errors: return Ok with advisory_errors so caller can report them
        // without blocking pipeline-type propagation.
        Ok((Rc::new(result_env), result_type, advisory_errors))
    } else {
        // Fatal body errors: still append advisory errors so all errors are reported together.
        errors.append(&mut advisory_errors);
        Err(errors)
    }
}

fn register_type_aliases(
    expr: &Spanned<Expr>,
    target_env: &mut TypeEnv,
    resolve_env: &TypeEnv,
    state: &mut InferState,
) -> Vec<TypeError> {
    let mut errors = Vec::new();
    if let Expr::Dict(entries) = &expr.node {
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let Expr::Str(name) = &key.node {
                    if let Expr::TypeAlias(inner) = &entry.node.value.node {
                        // Use a fresh per-alias mapping so annotation names within one type
                        // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
                        // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
                        // creates a distinct fresh var, breaking identity-function types.
                        let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                        let mut alias_row_map: HashMap<String, String> = HashMap::new();
                        match resolve_type_expr(
                            inner,
                            resolve_env,
                            state,
                            &mut Some(&mut alias_ann_map),
                            &mut Some(&mut alias_row_map),
                        ) {
                            Ok(alias_ty) => {
                                target_env.insert_type_alias(name.clone(), alias_ty);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                }
            }
        }
    }
    errors
}

fn infer_expr(
    expr: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let result = match &expr.node {
        Expr::Int(n) => Ok(Type::IntLiteral(*n)),
        Expr::Float(_) => Ok(Type::Float),
        Expr::Bool(_) => Ok(Type::Bool),
        Expr::Str(s) => Ok(Type::StringLiteral(s.clone())),

        Expr::VarRef(name) => env
            .get(name)
            .map(|scheme| instantiate_scheme(scheme, state.level, state))
            .ok_or_else(|| vec![TypeError::undefined_variable(name, expr.span)]),

        Expr::Dict(entries) => {
            infer_dict(entries, env, state, type_map, expr.span).map(|(ty, _schemes)| ty)
        }

        Expr::DotAccess {
            expr: target,
            field,
        } => check_dot_access(target, field, env, expr.span, state, type_map),

        Expr::BracketAccess { expr: target, key } => {
            check_bracket_access(target, key, env, expr.span, state, type_map)
        }

        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => check_range_access(target, start, end, env, expr.span, state, type_map),

        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => {
            // Special case: if func is a VarRef to a polymorphic scheme, pass the scheme
            // directly to avoid double instantiation (VAR-POLY followed by CALL-POLY).
            // For monomorphic schemes, use the normal path which handles TypeVar during letrec.
            if let Expr::VarRef(name) = &func.node {
                match env.get(name) {
                    Some(scheme) if !scheme.type_vars.is_empty() || !scheme.row_vars.is_empty() => {
                        // Polymorphic scheme: optimize by instantiating once in check_call_with_scheme
                        check_call_with_scheme(
                            scheme, func.span, args, named_args, env, expr.span, state, type_map,
                        )
                    }
                    Some(_) => {
                        // TypeVar: handles letrec forward-references where Pass 1 assigns TypeVar
                        // placeholders not yet generalized. The monomorphic path (check_call) reaches
                        // the TypeVar arm which infers args for side effects and returns Any, deferring
                        // type resolution until all letrec bindings have been inferred.
                        check_call(func, args, named_args, env, expr.span, state, type_map)
                    }
                    None => {
                        // Special handling for $proxy builtin: produces Type::Proxy
                        if name == "proxy" {
                            // Infer arguments for type map population
                            for arg in args {
                                let _ = infer_expr(arg, env, state, type_map)?;
                            }
                            for na in named_args {
                                let _ = infer_expr(&na.node.value, env, state, type_map)?;
                            }
                            Ok(Type::Proxy)
                        } else {
                            Err(vec![TypeError::undefined_variable(name, func.span)])
                        }
                    }
                }
            } else {
                check_call(func, args, named_args, env, expr.span, state, type_map)
            }
        }

        Expr::Fn {
            return_ann,
            params,
            body,
            ..
        } => infer_fn(return_ann, params, body, env, expr.span, state, type_map),

        Expr::TypeAlias(inner) => expand_type_alias(inner, env, state).map_err(|e| vec![e]),

        Expr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => resolve_type_assert(
            annotation,
            inner,
            resolved_type,
            env,
            expr.span,
            state,
            type_map,
        ),

        Expr::Annotated { name, annotation } => {
            resolve_annotated(name, annotation, env, expr.span, state, &mut None)
                .map_err(|e| vec![e])
        }

        Expr::Rest(_) => Err(vec![TypeError::new(
            "rest marker (...) is only valid inside type expressions",
            expr.span,
        )]),

        Expr::Error(span) => Err(vec![TypeError::new(
            &format!(
                "syntax error at {}:{} (cannot typecheck error node)",
                span.start.line, span.start.column
            ),
            expr.span,
        )]),
    };

    // Record the inferred type in the type map (if collecting).
    // On error, record Type::Error as a sentinel so that LSP hover shows <error>
    // rather than no type at all, and parent expressions can see Error via the type_map
    // rather than inferring from a missing entry.
    if let Some(ref mut map) = type_map {
        let key = (expr.span.start.offset, expr.span.end.offset);
        match &result {
            Ok(ty) => {
                map.insert(key, ty.clone());
            }
            Err(_) => {
                map.insert(key, Type::Error);
            }
        }
    }

    result
}

/// Check that an expression has a compatible type with the expected type.
/// Uses bidirectional type checking: synthesize the expression's type via `infer_expr`,
/// then check subsumption via `is_subtype(actual, expected)`.
///
/// Per doc/06-type-inference.md §Bidirectional Typing, this is the [SUB] rule:
/// if `Γ ⊢ e ⇒ σ` and `σ <: τ`, then `Γ ⊢ e ⇐ τ`.
///
/// Special case for lambdas (doc/06 §[CHECK-FN]): when checking a function expression
/// against an expected function type, propagate the expected parameter types into the
/// lambda's parameter inference (Pierce & Turner 2000 lambda checking mode).
///
/// This function is used at checking positions where the expected type is fully concrete
/// (no type variables): CALL-MONO arguments, concrete return annotations (no TypeVars), and TypeAssert.
fn check_expr(
    expr: &Spanned<Expr>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    // Lambda checking mode: when checking a function expression against a function type,
    // propagate expected parameter types into the lambda.
    // Only applies when expected type is fully concrete after applying state.subst
    // (no unbound type variables) per doc/06 §[CHECK-FN].
    if let Expr::Fn {
        return_ann,
        params,
        body,
        ..
    } = &expr.node
    {
        if let Type::Function { .. } = expected {
            // Apply current substitution before checking for TypeVars — TypeVars that are
            // already bound in state.subst are effectively resolved. Without this, lambda
            // checking mode is blocked by TypeVars that have known types, falling through
            // to the less precise synthesize+subsume path.
            // Per Algorithm W (Damas & Milner, 1982): substitutions must be applied before
            // inspecting types, maintaining the substitution threading invariant.
            let resolved_expected = if state.subst.is_empty() {
                expected.clone()
            } else {
                state.subst.apply(expected)
            };
            // Only use lambda checking mode if expected type is fully concrete after applying subst
            if let Type::Function {
                params: ref expected_params,
                ret: ref expected_ret,
                variadic: _,
            } = resolved_expected
            {
                if !resolved_expected.has_inference_vars() {
                    // Create a fresh annotation mapping for this lambda to prevent
                    // cross-contamination of type variables.
                    // Only allocate if any param has an annotation or there's a return annotation.
                    let has_annotations =
                        params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some();
                    let mut ann_mapping = if has_annotations {
                        Some(HashMap::new())
                    } else {
                        None
                    };
                    let mut ann_mapping_opt = ann_mapping.as_mut();
                    // row_ann_mapping tracks named row variables per lambda scope (kinded separation).
                    let mut row_ann_mapping = if has_annotations {
                        Some(HashMap::new())
                    } else {
                        None
                    };
                    let mut row_ann_mapping_opt = row_ann_mapping.as_mut();

                    // Arity check
                    if params.len() != expected_params.len() {
                        return Err(vec![TypeError::new(
                            format!(
                                "arity mismatch: expected {} arguments, got {}",
                                expected_params.len(),
                                params.len()
                            ),
                            expr.span,
                        )]);
                    }

                    // Build parameter types: use expected types for unannotated params.
                    // For annotated params, verify the annotation is compatible with the expected
                    // type: expected_ty must be a subtype of the annotation (contravariant check).
                    // Example: expected Fn(Int→...) but param declared @String → Int <: String is
                    // false → error, because callers will pass Int but the body expects String.
                    let param_types: Vec<Type> = params
                        .iter()
                        .zip(expected_params.iter())
                        .map(|(p, expected_ty)| match &p.node.annotation {
                            Some(ann) => {
                                let resolved = resolve_annotation(
                                    &ann.node,
                                    env,
                                    ann.span,
                                    state,
                                    &mut ann_mapping_opt,
                                    &mut row_ann_mapping_opt,
                                )?;
                                // Contravariant check: expected param type must be subtype of annotation.
                                // When annotation contains type variables, use unification mode instead of
                                // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                                if resolved.has_inference_vars() {
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result =
                                        unify(expected_ty, &resolved, &mut subst, state, ann.span);
                                    state.subst = subst;
                                    result.map_err(|_e| {
                                        TypeError::new(
                                            format!("parameter annotation {resolved} is more restrictive than required type {expected_ty}"),
                                            ann.span
                                        )
                                    })?;
                                } else {
                                    if !Type::is_subtype(expected_ty, &resolved) {
                                        return Err(TypeError::new(
                                            format!("parameter annotation {resolved} is more restrictive than required type {expected_ty}"),
                                            ann.span
                                        ));
                                    }
                                }
                                Ok(resolved)
                            }
                            None => Ok(expected_ty.clone()),
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|e| vec![e])?;

                    // Build function environment with parameter bindings
                    let mut fn_env = TypeEnv::with_parent(env);
                    for (param, ty) in params.iter().zip(param_types.iter()) {
                        if param.node.variadic {
                            fn_env.insert(param.node.name.clone(), Type::Any);
                        } else {
                            fn_env.insert(param.node.name.clone(), ty.clone());
                        }
                    }
                    let fn_env = Rc::new(fn_env);

                    // Check body against expected return type (or infer if no return annotation)
                    match return_ann {
                        Some(ann) => {
                            let declared = resolve_annotation(
                                &ann.node,
                                env,
                                ann.span,
                                state,
                                &mut ann_mapping_opt,
                                &mut row_ann_mapping_opt,
                            )
                            .map_err(|e| vec![e])?;
                            // Check that declared return type is compatible with expected.
                            // When declared contains type variables, use unification mode instead of
                            // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                            if declared.has_inference_vars() {
                                let mut subst = std::mem::take(&mut state.subst);
                                let result =
                                    unify(&declared, expected_ret, &mut subst, state, ann.span);
                                state.subst = subst;
                                result.map_err(|_e| {
                                    vec![TypeError::type_mismatch(
                                        expected_ret,
                                        &declared,
                                        expr.span,
                                    )]
                                })?;
                            } else {
                                if !Type::is_subtype(&declared, expected_ret) {
                                    return Err(vec![TypeError::type_mismatch(
                                        expected_ret,
                                        &declared,
                                        expr.span,
                                    )]);
                                }
                            }
                            // Check body against declared return type
                            check_expr(body, &declared, &fn_env, state, type_map)?;
                        }
                        None => {
                            // No return annotation: check body against expected return type.
                            // Apply state.subst to expected_ret — parameter inference
                            // (annotation unification above) may have added NEW bindings to
                            // state.subst that target TypeVars in expected_ret. The initial
                            // state.subst.apply at the guard resolved pre-existing bindings,
                            // but annotation unification can create new ones.
                            //
                            // Currently a no-op: the !has_inference_vars() guard ensures expected_ret
                            // (from the resolved type) has no TypeVars. Annotation unification
                            // binds annotation-fresh TypeVars, not expected_ret TypeVars. Retained
                            // as a safety net per Algorithm W substitution threading invariant.
                            let applied_ret = if state.subst.type_map.is_empty()
                                && state.subst.row_map.is_empty()
                            {
                                *expected_ret.clone()
                            } else {
                                state.subst.apply(expected_ret)
                            };
                            check_expr(body, &applied_ret, &fn_env, state, type_map)?;
                        }
                    }

                    // Record the function type in the type map — use the resolved
                    // (subst-applied) type so the map contains concrete types.
                    // In lambda checking mode, type_map records the expected function type
                    // (resolved_expected), not the synthesized type. This is correct
                    // bidirectional semantics for LSP hover: the lambda's type is determined
                    // by the checking context, not inferred from the body alone.
                    if let Some(ref mut map) = type_map {
                        let key = (expr.span.start.offset, expr.span.end.offset);
                        map.insert(key, resolved_expected.clone());
                    }

                    return Ok(());
                }
            }
        }
    }

    // Default: synthesize then check subsumption
    let actual = infer_expr(expr, env, state, type_map)?;
    // Apply state.subst to both types before comparison — access-chain constraints
    // may have bound TypeVars in state.subst. Without substitution, the comparison
    // uses stale TypeVars.
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let (actual, expected_resolved) = if state.subst.is_empty() {
        (actual, expected.clone())
    } else {
        (state.subst.apply(&actual), state.subst.apply(expected))
    };
    if !Type::is_subtype(&actual, &expected_resolved) {
        Err(vec![TypeError::type_mismatch(
            &expected_resolved,
            &actual,
            expr.span,
        )])
    } else {
        Ok(())
    }
}

fn infer_dict(
    entries: &[Spanned<Entry>],
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    span: Span,
) -> Result<(Type, HashMap<String, TypeScheme>), Vec<TypeError>> {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let mut dict_env = TypeEnv::with_parent(env);
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, state, type_map);
        let is_alias = matches!(&entry.node.value.node, Expr::TypeAlias(_));
        key_entries.push((key_name, is_alias));
    }

    // Pass 1: Bind all non-alias entries to fresh TypeVar at level state.level.
    // Also collect fresh vars into a local HashMap for direct O(1) lookup in Pass 3,
    // bypassing the TypeEnv parent-chain traversal in TypeEnv::get().
    let mut fresh_vars: HashMap<String, Type> = HashMap::new();
    for (key_name, is_alias) in &key_entries {
        if !is_alias {
            if let Some(ref name) = key_name {
                let fresh_var = state.fresh_type_var();
                fresh_vars.insert(name.clone(), fresh_var.clone());
                dict_env.insert_scheme(name.clone(), TypeScheme::mono(fresh_var));
            }
        }
    }

    // Pass 2: Register type aliases
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let Some(name) = key_name {
                if let Expr::TypeAlias(inner) = &entry.node.value.node {
                    // Use a fresh per-alias mapping so annotation names within one type
                    // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
                    // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
                    // creates a distinct fresh var, breaking identity-function types.
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    let mut alias_row_map: HashMap<String, String> = HashMap::new();
                    if let Ok(alias_ty) = resolve_type_expr(
                        inner,
                        &dict_env,
                        state,
                        &mut Some(&mut alias_ann_map),
                        &mut Some(&mut alias_row_map),
                    ) {
                        dict_env.insert_type_alias(name.clone(), alias_ty);
                    }
                }
            }
        }
    }

    let dict_env = Rc::new(dict_env);

    // Pass 3a: Initialize local substitution with bindings from state.subst.
    // Algorithm W threads a single substitution through inference. The two-substitution
    // model (local subst + state.subst) is a borrow-checker workaround. We initialize the
    // local subst with state.subst bindings so that letrec unification can see access-chain
    // constraints generated during value inference.
    let mut subst = Substitution {
        type_map: state.subst.type_map.clone(),
        row_map: state.subst.row_map.clone(),
    };

    // Pass 3: Infer values and unify with bound type vars
    let mut field_types: HashMap<String, Type> = HashMap::new();
    let mut errors = Vec::new();

    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias || matches!(&entry.node.value.node, Expr::Rest(_)) {
            continue;
        }
        if let Some(name) = key_name {
            match infer_expr(&entry.node.value, &dict_env, state, type_map) {
                Ok(value_ty) => {
                    // Get the bound TypeVar from Pass 1 via direct HashMap lookup,
                    // avoiding TypeEnv parent-chain traversal.
                    if let Some(bound_var) = fresh_vars.get(name.as_str()) {
                        // Unify the inferred type with the bound var
                        if let Err(e) = unify(
                            bound_var,
                            &value_ty,
                            &mut subst,
                            state,
                            entry.node.value.span,
                        ) {
                            errors.push(e);
                            field_types.insert(name.clone(), Type::Any);
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    } else {
                        field_types.insert(name.clone(), value_ty);
                    }
                }
                Err(mut errs) => {
                    errors.append(&mut errs);
                    field_types.insert(name.clone(), Type::Any);
                    // Populate type_map with Any for LSP hover on failed dict value expressions
                    if let Some(ref mut map) = type_map {
                        let key = (
                            entry.node.value.span.start.offset,
                            entry.node.value.span.end.offset,
                        );
                        map.insert(key, Type::Any);
                    }
                }
            }
        }
    }

    // Pass 3b: Merge bindings from state.subst added during value inference.
    // Algorithm W substitution composition (Damas & Milner 1982): correct composition
    // S = S_state . S_local requires unifying overlapping bindings, not discarding one.
    // The previous or_insert pattern dropped state.subst bindings when local subst already
    // had the same key, leaving access-chain constraints unresolved as free TypeVars.
    {
        let state_type_entries: Vec<(String, Type)> = state
            .subst
            .type_map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in state_type_entries {
            let applied_v = subst.apply(&v);
            match subst.type_map.get(&k).cloned() {
                Some(existing) => {
                    // Remove binding before calling unify to prevent apply() from chasing
                    // k -> existing -> k in a cycle during resolution (mirrors row_map path).
                    subst.type_map.remove(&k);
                    // Both maps bind the same variable: unify to reconcile constraints
                    unify(&existing, &applied_v, &mut subst, state, span).map_err(|e| vec![e])?;
                }
                None => {
                    subst.type_map.insert(k, applied_v);
                }
            }
        }
    }

    // For row_map: apply local subst to field types in state.subst row bindings, then merge.
    // Algorithm W substitution composition: unify on collision (same as type_map above).
    {
        let state_row_entries: Vec<(String, Row)> = state
            .subst
            .row_map
            .iter()
            .map(|(k, row)| (k.clone(), row.clone()))
            .collect();

        // Reusable HashMap to avoid allocation per iteration
        let mut applied_fields: HashMap<String, Type> = HashMap::new();

        for (k, row) in state_row_entries {
            applied_fields.clear();
            for (field_name, field_ty) in &row.fields {
                applied_fields.insert(field_name.clone(), subst.apply(field_ty));
            }
            let applied_row = Row {
                fields: applied_fields.clone(),
                // Tail not applied here; Pass 3c's subst.apply() chases tail chains transitively.
                tail: row.tail.clone(),
            };
            match subst.row_map.get(&k).cloned() {
                Some(existing) => {
                    // Both maps bind the same row variable: unify to reconcile constraints.
                    // Remove the binding for k before calling unify to prevent apply() from
                    // chasing k -> existing -> k in an infinite cycle during resolution.
                    subst.row_map.remove(&k);
                    unify(
                        &Type::Record(existing),
                        &Type::Record(applied_row),
                        &mut subst,
                        state,
                        span,
                    )
                    .map_err(|e| vec![e])?;
                }
                None => {
                    subst.row_map.insert(k, applied_row);
                }
            }
        }
    }

    // Pass 3c: Apply the merged substitution to all field types
    let field_types: HashMap<String, Type> = if subst.is_empty() {
        // Fast path: no substitution needed, avoid O(n) apply() calls
        field_types
    } else {
        field_types
            .into_iter()
            .map(|(k, ty)| (k, subst.apply(&ty)))
            .collect()
    };

    // Pass 3d: Merge local subst back into state.subst so that subsequent dict entries
    // in the same document can see the letrec bindings from this dict.
    // Without this, access-chain constraints in later dicts won't resolve TypeVars
    // that were bound during this dict's letrec unification.
    for (k, v) in &subst.type_map {
        state.subst.type_map.insert(k.clone(), v.clone());
    }
    for (k, row) in &subst.row_map {
        state.subst.row_map.insert(k.clone(), row.clone());
    }
    state.subst.check_size(span).map_err(|e| vec![e])?;

    // Pass 4: Generalize - create TypeSchemes for each entry
    let mut schemes = HashMap::with_capacity(field_types.len());
    for (name, ty) in &field_types {
        let scheme = generalize(enclosing_level, ty, state);
        schemes.insert(name.clone(), scheme);
    }

    // Restore enclosing level
    state.level = enclosing_level;

    let record_type = Type::Record(Row {
        fields: field_types,
        tail: RowTail::Empty,
    });

    if errors.is_empty() {
        Ok((record_type, schemes))
    } else {
        Err(errors)
    }
}

fn entry_key_name(
    entry: &Entry,
    auto_index: &mut i64,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_expr) => match &key_expr.node {
            Expr::Str(s) => Some(s.clone()),
            Expr::Int(n) => Some(n.to_string()),
            _ => match infer_expr(key_expr, env, state, type_map) {
                Ok(Type::StringLiteral(s)) => Some(s),
                Ok(Type::IntLiteral(n)) => Some(n.to_string()),
                _ => None,
            },
        },
        None => {
            let name = auto_index.to_string();
            *auto_index += 1;
            Some(name)
        }
    }
}

fn check_dot_access(
    target: &Spanned<Expr>,
    field: &str,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution so that constraints from prior accesses
    // on the same target are visible (doc/07-type-extensions.md Part 5).
    let target_ty = state.subst.apply(&target_ty);
    match target_ty {
        Type::Record(Row {
            ref fields,
            ref tail,
        }) => match fields.get(field) {
            Some(ty) => Ok(ty.clone()),
            None => match tail {
                // Open record (RowVar tail) and field not found: bind ρ → Row({field: β}, RowVar(ρ_fresh))
                // This records the constraint "ρ must contain field with type β".
                RowTail::RowVar(rho, rho_level_creation) => {
                    // Get the current level from state.levels (the source of truth after level lowering).
                    // The level in RowTail is the creation-time level; state.levels is the current (possibly lowered) level.
                    // See doc/06-type-inference.md §Let-Generalization (Levels-Based).
                    let rho_level = state.levels.get(rho).copied().unwrap_or(0);

                    // Invariant check: current level should be ≤ creation-time level (level lowering can only decrease levels)
                    debug_assert!(
                        rho_level <= *rho_level_creation,
                        "RowVar current level ({}) should be ≤ creation level ({}). \
                         Level lowering can only decrease levels, never increase. \
                         RowVar: {}, state.levels: {:?}",
                        rho_level,
                        rho_level_creation,
                        rho,
                        &state.levels
                    );

                    // Create fresh type var β for the field type
                    let beta = state.fresh_type_var();
                    // Create fresh row var ρ_fresh for the remaining tail
                    let (rho_fresh_name, rho_fresh_level) = state.fresh_row_var_name();

                    // Build the row to bind: Row({ field: β }, RowVar(ρ_fresh))
                    let mut new_fields = HashMap::new();
                    new_fields.insert(field.to_string(), beta.clone());
                    let binding = Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(rho_fresh_name, rho_fresh_level),
                    };

                    // Occurs check: ρ must not appear in the row being bound
                    // (uses state.subst to chase through any existing bindings)
                    //
                    // PROOF SKETCH (why this is unreachable with fresh variables):
                    //   For ρ to occur in Row({field: β}, RowVar(ρ_fresh)), either:
                    //     (a) β contains ρ in its structure, OR
                    //     (b) ρ_fresh = ρ
                    //
                    //   Both are impossible:
                    //     - β is fresh (line 696) with no prior bindings → cannot contain ρ
                    //     - ρ_fresh is fresh (line 698) → ρ_fresh ≠ ρ by uniqueness
                    //
                    //   Therefore, this check is defensive programming that documents the invariant
                    //   but cannot fail when the binding uses only fresh variables.
                    //
                    // Defense-in-depth: Keep the check to guard against future refactorings.
                    if row_var_occurs_pub(rho, &binding, &state.subst) {
                        debug_assert!(
                            false,
                            "unreachable: fresh row var ρ_fresh and fresh type var β cannot contain ρ. \
                             If this fires, check_dot_access was modified to use non-fresh variables."
                        );
                        return Err(vec![TypeError::new(
                            format!("infinite row type: {rho} occurs in its own binding"),
                            span,
                        )]);
                    }

                    // Level lowering: lower all vars in the binding to ρ's current level (from state.levels)
                    lower_row_var_levels_pub(&binding, rho_level, state);

                    // Bind ρ → binding in the global substitution
                    state.subst.row_map.insert(rho.clone(), binding);
                    state.subst.check_size(span).map_err(|e| vec![e])?;

                    Ok(beta)
                }
                // Closed record (Empty tail) and field not found: error
                RowTail::Empty => Err(vec![TypeError::field_not_found(field, &target_ty, span)]),
            },
        },
        // Unknown type (TypeVar α): generate constraint α = Record({field: β}, RowVar(ρ))
        // This records "α must be a record with at least this field".
        Type::TypeVar(ref alpha, alpha_level) => {
            // Create fresh β for the field type and ρ for the remaining row
            let beta = state.fresh_type_var();
            let (rho_name, rho_level) = state.fresh_row_var_name();

            // Build the record type to unify α with
            let mut fields = HashMap::new();
            fields.insert(field.to_string(), beta.clone());
            let record_ty = Type::Record(Row {
                fields,
                tail: RowTail::RowVar(rho_name, rho_level),
            });

            // Unify TypeVar(α) with Record({field: β}, RowVar(ρ)) using the global substitution.
            // Use mem::take to work around borrow checker (unify needs &mut subst and &mut state)
            let alpha_ty = Type::TypeVar(alpha.clone(), alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;

            Ok(beta)
        }
        Type::Any => Ok(Type::Any),
        Type::Proxy => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

// NOTE: The match in this function has a guarded TypeVar arm (static_field.is_some()) and an
// unguarded TypeVar catch-all arm (static_field.is_none()). The guard structure prevents the
// unguarded arm from shadowing the guarded one. If the Expr enum gains new variants, the match
// may silently fall to the catch-all — add explicit arms for new variants as they are added.
fn check_bracket_access(
    target: &Spanned<Expr>,
    key: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution (same pattern as check_dot_access).
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let target_ty = if state.subst.is_empty() {
        target_ty
    } else {
        state.subst.apply(&target_ty)
    };
    let key_ty = infer_expr(key, env, state, type_map)?;

    // Extract the string-literal field name from the key expression (if statically known).
    let static_field: Option<String> = match &key.node {
        Expr::Str(s) => Some(s.clone()),
        Expr::Int(n) => Some(n.to_string()),
        _ => match &key_ty {
            Type::StringLiteral(s) => Some(s.clone()),
            Type::IntLiteral(n) => Some(n.to_string()),
            _ => None,
        },
    };

    match &target_ty {
        Type::Record(Row {
            ref fields,
            ref tail,
        }) => {
            // If key is statically known, try field lookup first.
            if let Some(ref field_name) = static_field {
                if let Some(ty) = fields.get(field_name.as_str()) {
                    return Ok(ty.clone());
                }
                // Field not found — dispatch on tail (mirrors check_dot_access).
                match tail {
                    // Open record (RowVar tail): bind ρ → Row({field: β}, RowVar(ρ_fresh))
                    RowTail::RowVar(rho, rho_level_creation) => {
                        let rho_level = state.levels.get(rho).copied().unwrap_or(0);
                        debug_assert!(
                            rho_level <= *rho_level_creation,
                            "RowVar current level ({}) should be ≤ creation level ({})",
                            rho_level,
                            rho_level_creation,
                        );

                        let beta = state.fresh_type_var();
                        let (rho_fresh_name, rho_fresh_level) = state.fresh_row_var_name();

                        let mut new_fields = HashMap::new();
                        new_fields.insert(field_name.clone(), beta.clone());
                        let binding = Row {
                            fields: new_fields,
                            tail: RowTail::RowVar(rho_fresh_name, rho_fresh_level),
                        };

                        if row_var_occurs_pub(rho, &binding, &state.subst) {
                            debug_assert!(
                                false,
                                "unreachable: fresh row var and fresh type var cannot contain ρ"
                            );
                            return Err(vec![TypeError::new(
                                format!("infinite row type: {rho} occurs in its own binding"),
                                span,
                            )]);
                        }

                        lower_row_var_levels_pub(&binding, rho_level, state);
                        state.subst.row_map.insert(rho.clone(), binding);
                        state.subst.check_size(span).map_err(|e| vec![e])?;

                        return Ok(beta);
                    }
                    // Closed record (Empty tail): field not found error.
                    RowTail::Empty => {
                        return Err(vec![TypeError::field_not_found(
                            field_name, &target_ty, span,
                        )]);
                    }
                }
            }
            // Dynamic key — cannot generate field-level constraints.
            match &key_ty {
                Type::Str | Type::Int | Type::Number | Type::Any | Type::TypeVar(_, _) => {
                    Ok(Type::Any)
                }
                _ => Err(vec![TypeError::new(
                    format!("bracket access key must be String or Int, got {key_ty}"),
                    span,
                )]),
            }
        }
        // Unknown type (TypeVar α) with static key: generate constraint α = Record({key: β}, RowVar(ρ))
        Type::TypeVar(ref alpha, alpha_level) if static_field.is_some() => {
            let field_name =
                static_field.expect("static_field.is_some() guaranteed by match guard");
            let beta = state.fresh_type_var();
            let (rho_name, rho_level) = state.fresh_row_var_name();

            let mut fields = HashMap::new();
            fields.insert(field_name, beta.clone());
            let record_ty = Type::Record(Row {
                fields,
                tail: RowTail::RowVar(rho_name, rho_level),
            });

            let alpha_ty = Type::TypeVar(alpha.clone(), *alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;

            Ok(beta)
        }
        // Dynamic key (static_field.is_none()) or Any — cannot generate field-level constraints
        // without knowing the field name at inference time.
        Type::Any | Type::TypeVar(_, _) => Ok(Type::Any),
        Type::Proxy => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

fn check_range_access(
    target: &Spanned<Expr>,
    start: &Option<Box<Spanned<Expr>>>,
    end: &Option<Box<Spanned<Expr>>>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution (same pattern as check_dot_access).
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let target_ty = if state.subst.is_empty() {
        target_ty
    } else {
        state.subst.apply(&target_ty)
    };

    for bound in [start, end].into_iter().flatten() {
        let bound_ty = infer_expr(bound, env, state, type_map)?;
        if !matches!(
            bound_ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Any
                | Type::TypeVar(_, _)
        ) {
            return Err(vec![TypeError::new(
                format!("range bound must be Int or String, got {bound_ty}"),
                bound.span,
            )]);
        }
    }

    match &target_ty {
        Type::Record(..) | Type::Any | Type::TypeVar(_, _) => Ok(target_ty),
        Type::Proxy => Err(vec![TypeError::new(
            "range access is not supported on Proxy values",
            span,
        )]),
        Type::Seq(_) => Err(vec![TypeError::new(
            "range access is not supported on Seq types",
            span,
        )]),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Check a call where the function is a TypeScheme (from a VarRef lookup).
/// This avoids double instantiation: instead of VAR-POLY instantiating the scheme
/// and then CALL-POLY instantiating the result, we instantiate once here.
fn check_call_with_scheme(
    scheme: &TypeScheme,
    func_span: Span,
    args: &[Rc<Spanned<Expr>>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Instantiate the scheme once at the current level.
    // instantiate_at_level uses the call-site level (state.level) to ensure fresh type vars
    // are created at the correct generalization depth: vars at depth > enclosing_level will
    // be generalized by the enclosing let binding, while vars at shallower depth won't be.
    let func_ty = instantiate_scheme(scheme, state.level, state);

    // Record the function expression's type in the type map for LSP hover.
    // This mirrors check_dot_access recording the target span (line ~835).
    // check_call handles this via infer_expr, which records to type_map automatically.
    // check_call_with_scheme bypasses infer_expr (to avoid double instantiation), so
    // we must record explicitly here.
    if let Some(ref mut tm) = type_map {
        let key = (func_span.start.offset, func_span.end.offset);
        tm.insert(key, func_ty.clone());
    }

    // Infer named args for type map population and error propagation.
    // Named arg errors (e.g., undefined variable in a named arg value) must be propagated —
    // cascade prevention only applies to positional args where an Error arg would cause
    // spurious "wrong argument type" unification errors.
    //
    // TODO(named-arg-types): Named arg types are not unified against the corresponding param types.
    // This requires `Type::Function` to carry param names alongside param types so that a named
    // arg `x: e` can be matched to the param at the index where `param.name == "x"`. Until
    // `Type::Function` is extended to `params: Vec<(String, Type)>` (or equivalent), type
    // mismatches in named args are silently accepted by the type checker even though the evaluator
    // validates them at runtime (see `eval_call.rs` C-NAMED-VALID check).
    if !named_args.is_empty() {
        let mut named_arg_errors: Vec<TypeError> = Vec::new();
        for na in named_args {
            if let Err(mut errs) = infer_expr(&na.node.value, env, state, type_map) {
                named_arg_errors.append(&mut errs);
            }
        }
        if !named_arg_errors.is_empty() {
            return Err(named_arg_errors);
        }
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
            // TODO(named-arg-types): When Type::Function carries param names, validate that each
            // named arg targets a real param and doesn't overlap a positional arg position.
            let total_supplied = args.len() + named_args.len();
            // For variadic functions, the last param accepts arbitrary extra args.
            // Require at least (params.len() - 1) args for variadic functions.
            let min_required = if *variadic && !params.is_empty() {
                params.len() - 1
            } else {
                params.len()
            };
            if total_supplied < min_required || (!*variadic && total_supplied != params.len()) {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {}{} argument(s), got {} ({} positional, {} named)",
                        if *variadic { "at least " } else { "" },
                        min_required,
                        total_supplied,
                        args.len(),
                        named_args.len(),
                    ),
                    span,
                )]);
            }

            // CALL-POLY: After instantiation, the function type always has type variables.
            // This is guaranteed by the guard at line 236: check_call_with_scheme is only called
            // for polymorphic schemes (non-empty type_vars or row_vars), and instantiate_scheme
            // produces fresh TypeVars/RowVars for each quantified variable. Since generalize only
            // quantifies variables that appear in the body, the instantiated type must contain
            // those fresh variables, so has_inference_vars() is always true.
            // Synthesize arguments and unify (doc/06 §[CALL-POLY])
            //
            // Cascade prevention: if an argument fails inference, use Type::Error as its type
            // (the error has already been recorded in type_map by infer_expr) rather than
            // propagating the error immediately. Collect all argument errors, then report them.
            // unify(Error, param_ty) = Ok(()) by the Error-absorption rule in unify(), so the
            // rest of argument unification continues without spurious additional errors.
            debug_assert!(
                func_ty.has_inference_vars(),
                "check_call_with_scheme: func_ty must have inference vars after instantiation (invariant violated)"
            );
            let mut arg_types = Vec::with_capacity(args.len());
            let mut arg_errors: Option<Vec<TypeError>> = None;
            for a in args {
                match infer_expr(a, env, state, type_map) {
                    Ok(ty) => arg_types.push(ty),
                    Err(mut errs) => {
                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        arg_types.push(Type::Error);
                    }
                }
            }

            if !params.is_empty() {
                // Seed local subst from state.subst so that unification sees access-chain
                // constraints and letrec bindings accumulated by prior inference steps.
                // This mirrors infer_dict Pass 3a (lines 553-561): Algorithm W threads a
                // single substitution through inference; the two-substitution model is a
                // borrow-checker workaround. Without seeding, param_ty is unified against
                // arg_ty in an empty substitution context, missing bindings for TypeVars
                // that state.subst already resolved (Damas & Milner 1982, Theorem 2).
                //
                // Fresh type vars from instantiate_scheme are call-site-local and should not escape.
                // The local substitution is consumed by subst.apply(ret) and does not need to propagate
                // upstream — only the constraints accumulated during argument unification (merged back
                // into state.subst at lines 1475-1480) need to be visible to downstream inference.
                let mut subst = Substitution {
                    type_map: state.subst.type_map.clone(),
                    row_map: state.subst.row_map.clone(),
                };
                for (param_ty, arg_ty) in params.iter().zip(arg_types.iter()) {
                    // Error-typed args absorb silently (unify(Error, T) = Ok(())),
                    // so we only propagate unification errors from non-Error args.
                    if let Err(e) = unify(param_ty, arg_ty, &mut subst, state, span) {
                        arg_errors.get_or_insert_with(Vec::new).push(e);
                    }
                }
                if let Some(errors) = arg_errors {
                    return Err(errors);
                }
                // Merge local subst back into state.subst so that constraints from this
                // polymorphic call site are visible to subsequent inference steps. Without
                // this merge, bindings accumulated during argument unification (e.g., a
                // TypeVar constrained to Int) are lost for downstream entries in the same
                // letrec group. This mirrors infer_dict Pass 3d (lines 764-773).
                for (k, v) in &subst.type_map {
                    state.subst.type_map.insert(k.clone(), v.clone());
                }
                for (k, row) in &subst.row_map {
                    state.subst.row_map.insert(k.clone(), row.clone());
                }
                state.subst.check_size(span).map_err(|e| vec![e])?;
                // Apply state.subst only if non-empty (performance optimization)
                if state.subst.is_empty() {
                    Ok(subst.apply(ret))
                } else {
                    Ok(state.subst.apply(&subst.apply(ret)))
                }
            } else {
                // Zero-param: no arguments to unify, return type needs no substitution applied
                // from local argument unification (there are no arguments). Apply state.subst
                // for access-chain constraints that may bind type vars in the return type.
                if state.subst.is_empty() {
                    Ok((**ret).clone())
                } else {
                    Ok(state.subst.apply(ret))
                }
            }
        }
        Type::Any => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-POLY (infer_expr at line 934).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map);
            }
            Ok(Type::Any)
        }
        _ => Err(vec![TypeError::not_a_function(&func_ty, func_span)]),
    }
}

/// Check a function call expression.
///
/// Inline lambdas with type annotations (e.g., `[call [fn [x@a] $x] 42]`) go through
/// this function, not `check_call_with_scheme`, because the callee is a `Fn` expression
/// (not a `VarRef` to a polymorphic scheme). `infer_expr` on the `Fn` synthesizes a type
/// with fresh TypeVars from annotations, which then enters the CALL-POLY path for
/// instantiation. This is a double-instantiation (annotation TypeVars + CALL-POLY TypeVars)
/// but is harmless for single-call sites: the extra freshening produces equivalent
/// constraints. The `check_call_with_scheme` optimization (instantiate once) only applies
/// to `VarRef` callees where the scheme is looked up from the environment.
///
/// Note: Named argument type checking is partial — this function infers named arg value types for
/// LSP hover and error propagation, and counts named args toward arity, but cannot verify that
/// each named arg's name matches a real parameter (or unify the arg type against the param type).
/// Both require `Type::Function` to carry param names. See TODO(named-arg-types) inline comments.
fn check_call(
    func: &Spanned<Expr>,
    args: &[Rc<Spanned<Expr>>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let func_ty = infer_expr(func, env, state, type_map)?;
    // Apply state.subst to resolve any TypeVars bound during infer_expr (e.g., from infer_fn
    // with polymorphic return annotations). Without this, has_inference_vars() incorrectly returns
    // true for already-bound TypeVars, causing CALL-POLY to fire and double-instantiate.
    let func_ty = if state.subst.is_empty() {
        func_ty
    } else {
        state.subst.apply(&func_ty)
    };

    // Infer named args for type map population and error propagation.
    // Named arg errors (e.g., undefined variable in a named arg value) must be propagated —
    // cascade prevention only applies to positional args where an Error arg would cause
    // spurious "wrong argument type" unification errors.
    //
    // TODO(named-arg-types): Named arg types are not unified against the corresponding param types.
    // This requires `Type::Function` to carry param names alongside param types so that a named
    // arg `x: e` can be matched to the param at the index where `param.name == "x"`. Until
    // `Type::Function` is extended to `params: Vec<(String, Type)>` (or equivalent), type
    // mismatches in named args are silently accepted by the type checker even though the evaluator
    // validates them at runtime (see `eval_call.rs` C-NAMED-VALID check).
    if !named_args.is_empty() {
        let mut named_arg_errors: Vec<TypeError> = Vec::new();
        for na in named_args {
            if let Err(mut errs) = infer_expr(&na.node.value, env, state, type_map) {
                named_arg_errors.append(&mut errs);
            }
        }
        if !named_arg_errors.is_empty() {
            return Err(named_arg_errors);
        }
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
            // TODO(named-arg-types): When Type::Function carries param names, validate that each
            // named arg targets a real param and doesn't overlap a positional arg position.
            let total_supplied = args.len() + named_args.len();
            // For variadic functions, the last param accepts arbitrary extra args.
            // Require at least (params.len() - 1) args for variadic functions.
            let min_required = if *variadic && !params.is_empty() {
                params.len() - 1
            } else {
                params.len()
            };
            if total_supplied < min_required || (!*variadic && total_supplied != params.len()) {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {}{} argument(s), got {} ({} positional, {} named)",
                        if *variadic { "at least " } else { "" },
                        min_required,
                        total_supplied,
                        args.len(),
                        named_args.len(),
                    ),
                    span,
                )]);
            }

            // CALL-MONO: function type is fully concrete (no type variables)
            // Use bidirectional checking for arguments via [SUB] rule (doc/06 §[CALL-MONO])
            //
            // ASYMMETRY: CALL-MONO collects all argument errors before returning (errors Vec
            // accumulates then is returned at once), while CALL-POLY (below) stops at the first
            // unification failure (map_err returns immediately). CALL-MONO's multi-error approach
            // is preferred for user-facing type errors; CALL-POLY's early-exit is a limitation of
            // sequential unification where later argument types may be meaningless if earlier
            // unification fails (type variables left unbound). A future improvement would
            // collect CALL-POLY errors too, but requires constraint-based solving (see comment below).
            if !func_ty.has_inference_vars() {
                let mut errors = Vec::new();
                for (arg, param_ty) in args.iter().zip(params.iter()) {
                    if let Err(mut errs) = check_expr(arg, param_ty, env, state, type_map) {
                        errors.append(&mut errs);
                    }
                }
                if !errors.is_empty() {
                    return Err(errors);
                }
                // Apply state.subst for defensive consistency with check_call_with_scheme.
                // The CALL-MONO guard (!func_ty.has_inference_vars()) means ret is typically fully
                // concrete, making apply() a no-op. But applying defensively guards against
                // edge cases where has_inference_vars() and the substitution domain diverge.
                return Ok(state.subst.apply(ret));
            }

            // CALL-POLY: function type has type variables
            // Instantiate the function type, synthesize arguments, then unify (doc/06 §[CALL-POLY])
            let inst_ty = instantiate_at_level(&func_ty, state);

            let (inst_params, inst_ret) = match &inst_ty {
                Type::Function {
                    params,
                    ret,
                    variadic: _,
                } => (params, ret),
                _ => unreachable!("instantiate_at_level preserves Function variant"),
            };

            // Synthesize argument types for CALL-POLY (not checking mode).
            // Cascade prevention: if an argument fails inference, use Type::Error as its type
            // (the error has already been recorded in type_map by infer_expr) rather than
            // propagating the error immediately. Collect all argument errors, then report them.
            // unify(Error, param_ty) = Ok(()) by the Error-absorption rule in unify(), so the
            // rest of argument unification continues without spurious additional errors.
            let mut arg_types = Vec::with_capacity(args.len());
            let mut arg_errors: Option<Vec<TypeError>> = None;
            for a in args {
                match infer_expr(a, env, state, type_map) {
                    Ok(ty) => arg_types.push(ty),
                    Err(mut errs) => {
                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        arg_types.push(Type::Error);
                    }
                }
            }

            if !params.is_empty() {
                // Sequential unification of arguments (textbook Hindley-Milner)
                //
                // This approach unifies each (param_ty, arg_ty) pair in order, accumulating
                // a substitution that is applied to subsequent unifications. This is SOUND
                // because instantiation creates fresh type variables per call site, so
                // type variable bindings from earlier arguments are correctly propagated.
                //
                // CONFLUENCE: When multiple arguments constrain the same type variable with
                // different precision (e.g., IntLiteral(42) vs Int), the bidirectional promotion
                // rules in unify() (src/types.rs, "Literal-to-parent promotions") ensure that type checking SUCCEEDS
                // regardless of argument order. The only difference is the PRECISION of the
                // resulting binding:
                //
                //   Example: ∀a. Fn(a a → a) called with IntLiteral(42) and Int
                //   - Order 1: unify(_t0, IntLiteral(42)) → {_t0 ↦ IntLiteral(42)},
                //              then unify(IntLiteral(42), Int) → SUCCESS (promotion rule)
                //              Result: _t0 = IntLiteral(42)
                //   - Order 2: unify(_t0, Int) → {_t0 ↦ Int},
                //              then unify(Int, IntLiteral(42)) → SUCCESS (promotion rule)
                //              Result: _t0 = Int
                //
                // Both orderings succeed; the first is more precise. This is the expected
                // behavior: earlier arguments guide type variable binding, later arguments
                // validate compatibility via subsumption. The bidirectional promotion rules
                // (IntLiteral ↔ Int, StringLiteral ↔ Str, Int ↔ Number, etc.) make this
                // unification subsumptive in practice, providing the same confluence property
                // that Pierce-Turner's [U-SUBSUME] rule would provide (doc/06 §Subsumptive fallback).
                //
                // CONSTRAINT-BASED ALTERNATIVE: Pierce-Turner bidirectional typing suggests
                // collecting all constraints before solving (constraint generation, then
                // joint constraint solving). This would allow computing a "minimal" or
                // "maximal" substitution (e.g., least upper bound when multiple constraints
                // exist). However, for tinct's current type system:
                //
                //   1. Sequential unification is simpler and matches standard HM implementations
                //   2. The bidirectional promotion rules already handle the common cases
                //   3. Left-to-right evaluation order makes it natural for earlier args to
                //      guide type inference
                //   4. No test case currently demonstrates a need for constraint collection
                //
                // If future extensions (e.g., row-variable unification with constraints on
                // multiple row variables, or bidirectional typing with checking mode for
                // CALL-POLY args) require more sophisticated constraint solving, the switch
                // to constraint generation would happen here. The current approach is
                // intentionally pragmatic.
                //
                // Seed local subst from state.subst so that unification sees access-chain
                // constraints and letrec bindings accumulated by prior inference steps.
                // This mirrors check_call_with_scheme (lines 1086-1088) and infer_dict Pass 3a:
                // Algorithm W threads a single substitution through inference; the two-substitution
                // model is a borrow-checker workaround. Without seeding, param_ty is unified
                // against arg_ty in an empty substitution context, missing bindings for TypeVars
                // that state.subst already resolved (Damas & Milner 1982, Theorem 2).
                let mut subst = Substitution {
                    type_map: state.subst.type_map.clone(),
                    row_map: state.subst.row_map.clone(),
                };
                for (param_ty, arg_ty) in inst_params.iter().zip(arg_types.iter()) {
                    // Error-typed args absorb silently (unify(Error, T) = Ok(())),
                    // so we only propagate unification errors from non-Error args.
                    if let Err(e) = unify(param_ty, arg_ty, &mut subst, state, span) {
                        arg_errors.get_or_insert_with(Vec::new).push(e);
                    }
                }
                if let Some(errors) = arg_errors {
                    return Err(errors);
                }
                // Merge local subst back into state.subst so that constraints from this
                // polymorphic call site are visible to subsequent inference steps. Without
                // this merge, bindings accumulated during argument unification (e.g., a
                // TypeVar constrained to Int) are lost for downstream entries in the same
                // letrec group. This mirrors check_call_with_scheme (lines 1098-1104) and
                // infer_dict Pass 3d (lines 764-773).
                for (k, v) in &subst.type_map {
                    state.subst.type_map.insert(k.clone(), v.clone());
                }
                for (k, row) in &subst.row_map {
                    state.subst.row_map.insert(k.clone(), row.clone());
                }
                state.subst.check_size(span).map_err(|e| vec![e])?;
                // Apply state.subst only if non-empty (performance optimization)
                if state.subst.is_empty() {
                    Ok(subst.apply(inst_ret))
                } else {
                    Ok(state.subst.apply(&subst.apply(inst_ret)))
                }
            } else {
                // Zero-param polymorphic function: return the instantiated return type
                // (not the original `ret` which contains the scheme-internal variable names)
                if state.subst.is_empty() {
                    Ok((**inst_ret).clone())
                } else {
                    Ok(state.subst.apply(inst_ret))
                }
            }
        }
        Type::TypeVar(_, _) => {
            // Unbound type variable (e.g. letrec forward reference to a function not yet
            // inferred). state.subst.apply already resolved bound TypeVars (line 1140-1144),
            // so reaching here means alpha is genuinely unbound. Conservative fallback:
            // infer args for side effects and return Any.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map);
            }
            Ok(Type::Any)
        }
        Type::Any => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-MONO (check_expr at line 1011) or CALL-POLY (infer_expr).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map);
            }
            Ok(Type::Any)
        }
        _ => Err(vec![TypeError::not_a_function(&func_ty, span)]),
    }
}

fn infer_fn(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<Param>],
    body: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Create a fresh annotation mapping for this function to prevent
    // cross-contamination of type variables.
    // Only allocate if any param has an annotation or there's a return annotation.
    // This guard is a performance optimization only: if there are no annotations,
    // resolve_annotation is never called (it receives Type::Any directly), so an empty
    // HashMap would never be consulted. Skipping allocation has no behavior impact.
    let has_annotations =
        params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some();
    let mut ann_mapping = if has_annotations {
        Some(HashMap::new())
    } else {
        None
    };
    let mut ann_mapping_opt = ann_mapping.as_mut();
    // row_ann_mapping tracks named row variables (e.g., ...r in [a: Int ...r]) per function scope.
    // It is separate from ann_mapping (which tracks type-kind variables) to enforce kinded
    // substitution: a name used as a row variable cannot also be used as a type variable.
    let mut row_ann_mapping = if has_annotations {
        Some(HashMap::new())
    } else {
        None
    };
    let mut row_ann_mapping_opt = row_ann_mapping.as_mut();

    let mut param_types: Vec<Type> = params
        .iter()
        .map(|p| match &p.node.annotation {
            Some(ann) => resolve_annotation(
                &ann.node,
                env,
                ann.span,
                state,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
            ),
            None => Ok(Type::Any),
        })
        .collect::<Result<_, _>>()
        .map_err(|e| vec![e])?;

    let mut fn_env = TypeEnv::with_parent(env);
    for (i, param) in params.iter().enumerate() {
        if param.node.variadic {
            let variadic_ty = Type::Any;
            // Variadic params accept arbitrary fields, typed as Any.
            // Update param_types[i] to match the env binding so the function signature is accurate.
            param_types[i] = variadic_ty.clone();
            fn_env.insert(param.node.name.clone(), variadic_ty);
        } else {
            fn_env.insert(param.node.name.clone(), param_types[i].clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            let declared = resolve_annotation(
                &ann.node,
                env,
                ann.span,
                state,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
            )
            .map_err(|e| vec![e])?;

            // When declared return type contains type variables, switch to unification mode
            // (doc/06 §[CHECK-FN], Damas & Milner 1982, Pierce & Turner 2000 §3.2).
            // TypeVars in is_subtype only match via reflexive equality, so
            // is_subtype(IntLiteral(42), TypeVar("_t5")) = false would reject valid code.
            // Unification mode binds the TypeVars via constraint solving.
            if declared.has_inference_vars() {
                let body_ty = infer_expr(body, &fn_env, state, type_map)?;
                // Borrow-split: mem::take + restore avoids simultaneous &mut state.subst and &mut state
                let mut subst = std::mem::take(&mut state.subst);
                let result = unify(&body_ty, &declared, &mut subst, state, body.span);
                state.subst = subst;
                result.map_err(|e| vec![e])?;
                // Apply substitution to resolve any TypeVars bound during unification.
                // Without this, the returned Type::Function would have has_inference_vars() == true,
                // causing check_call to enter the CALL-POLY path unnecessarily (see check_call's
                // has_inference_vars guard). This prevents call sites from entering CALL-POLY.
                state.subst.apply(&declared)
            } else {
                // Use checking mode for concrete return types (no type variables)
                check_expr(body, &declared, &fn_env, state, type_map)?;
                declared
            }
        }
        None => infer_expr(body, &fn_env, state, type_map)?,
    };

    // Check if any parameter is variadic
    let has_variadic = params.iter().any(|p| p.node.variadic);

    Ok(Type::Function {
        params: param_types,
        ret: Box::new(ret_type),
        variadic: has_variadic,
    })
}

fn expand_type_alias(
    inner: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
) -> Result<Type, TypeError> {
    // Use a fresh per-alias mapping so annotation names within one type alias expression
    // (e.g., `a` in `[Fn@a [a]]`) consistently map to the same fresh TypeVar.
    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
    let mut alias_row_map: HashMap<String, String> = HashMap::new();
    // The `let _ = resolve_type_expr(...)` discards the resolved type intentionally — the call
    // is for validation side-effects (error propagation) only. Standalone type alias expressions
    // have no runtime type; returning Any is correct. The actual type alias definition is
    // registered in the TypeEnv during dict inference (see infer_dict Pass 2).
    let _ = resolve_type_expr(
        inner,
        env,
        state,
        &mut Some(&mut alias_ann_map),
        &mut Some(&mut alias_row_map),
    )?;
    Ok(Type::Any)
}

fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Spanned<Expr>,
    resolved_type: &RefCell<Option<Type>>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let expected = resolve_annotation(
        &annotation.node,
        env,
        annotation.span,
        state,
        &mut None,
        &mut None,
    )
    .map_err(|e| vec![e])?;

    // resolved_type will be stored after substitution application below (write-once invariant).

    // Use checking mode for TypeAssert inner expression (doc/06 §Bidirectional Typing)
    let check_result = check_expr(inner, &expected, env, state, type_map);

    // If checking fails and there's a default, suppress the error (ASSERT-DEFAULT rule)
    if check_result.is_err() {
        let has_default = annotation.node.get_property("default").is_some();
        if !has_default {
            return check_result.map(|_| expected);
        }
    }

    // Validate the default value type — hard error if the default cannot satisfy the asserted type.
    if let Some(default_expr) = annotation.node.get_property("default") {
        match infer_expr(default_expr, env, state, type_map) {
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
                if !Type::is_subtype(&default_ty, &expected_resolved) {
                    return Err(vec![TypeError::new(
                        format!(
                            "default value type mismatch: default has type {default_ty}, \
                             but assertion expects {expected_resolved}"
                        ),
                        default_expr.span,
                    )]);
                }
            }
            Err(errs) => {
                // Propagate inference errors from the default expression
                return Err(errs);
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
    let prev = resolved_type.replace(Some(expected.clone()));
    debug_assert!(
        prev.is_none(),
        "resolved_type written twice — elaboration invariant violated (span: {:?})",
        annotation.span
    );

    Ok(expected)
}

/// Resolve an annotated type expression `[@Name $annotation]`.
/// If `name == "Fn"`, interprets `$annotation` as a function type specification:
/// - `[@Fn@RetType [Param1 Param2 ...]]` → function type with params and return type
/// - `[@Fn@RetType]` (no param list) → zero-parameter function returning RetType
/// Otherwise, resolves `$annotation` as a regular type annotation.
fn resolve_annotated(
    name: &str,
    annotation: &Spanned<Annotation>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if name == "Fn" {
        resolve_fn_type(
            &annotation.node,
            env,
            annotation.span,
            state,
            ann_mapping,
            &mut None,
        )
    } else {
        resolve_annotation(&annotation.node, env, span, state, ann_mapping, &mut None)
    }
}

/// Resolve a bare `Fn@ReturnType` annotation (without parameter list) into a function type.
/// `Fn@T` bare = zero-param function returning T; full function type with params uses `try_resolve_fn_type_expr`.
fn resolve_fn_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    let ret = resolve_annotation_as_type(ann, env, span, state, ann_mapping, row_ann_mapping)?;
    Ok(Type::Function {
        params: vec![],
        ret: Box::new(ret),
        variadic: false,
    })
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
        Annotation::PropertyDict(entries) => {
            resolve_type_dict(entries, env, span, state, ann_mapping, row_ann_mapping)
        }
    }
}

fn resolve_annotation(
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
        Annotation::PropertyDict(entries) => {
            if let Some(type_val) = ann.get_property("type") {
                resolve_type_expr(type_val, env, state, ann_mapping, row_ann_mapping)
            } else {
                resolve_property_dict_as_record(
                    entries,
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
    entries: &[Spanned<Entry>],
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
            Ok(Type::Any)
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
fn entries_look_like_type_dict(entries: &[Spanned<Entry>]) -> bool {
    // Detect `[Fn@Return [Params]]` function type pattern: first entry is
    // auto-indexed with an Annotated node whose name is "Fn".
    if let Some(first) = entries.first() {
        if first.node.key.is_none() {
            if let Expr::Annotated { name, .. } = &first.node.value.node {
                if name == "Fn" {
                    return true;
                }
            }
        }
    }

    // Record type pattern: every entry has a string key and a type-shaped value.
    entries.iter().all(|entry| {
        // Rest entries (`...` / `...name`) are valid in type dicts
        if matches!(&entry.node.value.node, Expr::Rest(_)) {
            return true;
        }
        // Every entry must have a string key
        let has_str_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| matches!(&k.node, Expr::Str(_)));
        // Value must be a form that could be a type expression
        let value_is_type_shaped = matches!(
            &entry.node.value.node,
            Expr::Str(_) | Expr::VarRef(_) | Expr::Dict(_) | Expr::Annotated { .. }
        );
        has_str_key && value_is_type_shaped
    })
}

fn resolve_type_name(
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
        "String" => Ok(Type::Str),
        "Bool" => Ok(Type::Bool),
        "Number" => Ok(Type::Number),
        "Any" => Ok(Type::Any),
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
                // Cross-kind collision check (row→type direction): if the name was already
                // registered as a row variable (in row_ann_mapping), it cannot also be used
                // as a type variable. This is the symmetric counterpart to the type→row check
                // in resolve_type_dict (which checks ann_mapping before registering in
                // row_ann_mapping).
                let cross_kind_row = row_ann_mapping
                    .as_ref()
                    .map_or(false, |m| m.contains_key(name));
                if cross_kind_row {
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
                        state.name_counter += 1;
                        state.levels.insert(fresh.clone(), state.level);
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
                env.get_type_alias(name)
                    .cloned()
                    .ok_or_else(|| TypeError::undefined_type(name, span))
            }
        }
    }
}

fn resolve_type_expr(
    expr: &Spanned<Expr>,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Str(name) | Expr::VarRef(name) => {
            // Pass row_ann_mapping as read-only reference for cross-kind collision detection.
            let row_ref: Option<&HashMap<String, String>> = row_ann_mapping.as_ref().map(|m| &**m);
            resolve_type_name(name, env, expr.span, state, ann_mapping, &row_ref)
        }
        Expr::Dict(entries) => {
            resolve_type_dict(entries, env, expr.span, state, ann_mapping, row_ann_mapping)
        }
        Expr::Annotated { name, annotation } => {
            if name == "Fn" {
                resolve_fn_type(
                    &annotation.node,
                    env,
                    annotation.span,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            } else {
                resolve_annotation(
                    &annotation.node,
                    env,
                    expr.span,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )
            }
        }
        // This arm handles `Expr::Call { implied: true }`, which arises when a bare
        // identifier (no `@` annotation) appears in head position inside a type expression,
        // e.g. `[Fn [Int Int]]` (missing the required `@` before the return type).
        //
        // NOTE: The inner `if let Expr::Annotated` guard is currently unreachable.
        // `Fn@RetType` in head position is routed to `Expr::Dict` by the parser's Priority 2b
        // rule (Identifier + ImmediateAt → Dict), so the func of any `implied: true` Call is
        // always `Expr::VarRef`, never `Expr::Annotated`. The guard never fires; all implied
        // calls in type context fall through to the `Err(...)` at the end of this arm.
        Expr::Call {
            implied: true,
            func,
            args,
            ..
        } => {
            if let Expr::Annotated { name, annotation } = &func.node {
                if name == "Fn" {
                    // Fn@RetType [Params] in new syntax: resolve return type from annotation,
                    // then resolve each arg as a parameter type. For zero params, args is empty.
                    let ret = resolve_annotation_as_type(
                        &annotation.node,
                        env,
                        annotation.span,
                        state,
                        ann_mapping,
                        row_ann_mapping,
                    )?;
                    // args[0] should be the parameter list (a Dict or another implied Call)
                    let mut params = Vec::new();
                    if let Some(param_list) = args.first() {
                        match &param_list.node {
                            Expr::Dict(param_entries) => {
                                for entry in param_entries {
                                    params.push(resolve_type_expr(
                                        &entry.node.value,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )?);
                                }
                            }
                            Expr::Call {
                                implied: true,
                                func: inner_func,
                                args: inner_args,
                                ..
                            } => {
                                // Param list itself is an implied call: [a b c] → VarRef("a") + args
                                params.push(resolve_type_expr(
                                    inner_func,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?);
                                for a in inner_args.iter() {
                                    params.push(resolve_type_expr(
                                        a,
                                        env,
                                        state,
                                        ann_mapping,
                                        row_ann_mapping,
                                    )?);
                                }
                            }
                            _ => {
                                // Single param that's not a Dict
                                params.push(resolve_type_expr(
                                    param_list,
                                    env,
                                    state,
                                    ann_mapping,
                                    row_ann_mapping,
                                )?);
                            }
                        }
                    }
                    if args.len() > 1 {
                        return Err(TypeError::new(
                            format!(
                                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                                1 + args.len()
                            ),
                            expr.span,
                        ));
                    }
                    return Ok(Type::Function {
                        params,
                        ret: Box::new(ret),
                        variadic: false,
                    });
                }
            }
            Err(TypeError::new(
                format!("invalid type expression in annotation: {}", expr.node),
                expr.span,
            ))
        }
        _ => Err(TypeError::new(
            format!("invalid type expression in annotation: {}", expr.node),
            expr.span,
        )),
    }
}

fn resolve_type_dict(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
    row_ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if let Some(fn_type) =
        try_resolve_fn_type_expr(entries, env, span, state, ann_mapping, row_ann_mapping)?
    {
        return Ok(fn_type);
    }

    let mut fields: HashMap<String, Type> = HashMap::new();
    let mut rest = RowTail::Empty;
    for entry in entries {
        if let Expr::Rest(name) = &entry.node.value.node {
            rest = match name {
                None => {
                    // Anonymous open record: generate a fresh row variable name
                    let fresh_name = format!("_open{}", state.name_counter);
                    state.name_counter += 1;
                    state.levels.insert(fresh_name.clone(), state.level);
                    RowTail::RowVar(fresh_name, state.level)
                }
                Some(n) => {
                    // Row variables in type expressions use row_ann_mapping (not ann_mapping).
                    // ann_mapping is for type-kind variables; row_ann_mapping is for row-kind
                    // variables. Using the wrong map would cause cross-kind collisions where a
                    // name used as both a type variable and a row variable maps to the same fresh
                    // name in the type substitution (kinded substitution violation).
                    //
                    // Cross-kind collision: if the same name appears in both ann_mapping (as a
                    // type variable) and row_ann_mapping (as a row variable), the annotation is
                    // ambiguous and must be rejected.
                    let cross_kind = ann_mapping.as_ref().map_or(false, |m| m.contains_key(n));
                    if cross_kind {
                        // Same name already used as a type variable in this function scope.
                        // This is a cross-kind collision: reject with a TypeError.
                        return Err(TypeError::new(
                            format!(
                                "annotation name '{n}' is already used as a type variable in this function; \
                                 it cannot also be used as a row variable"
                            ),
                            span,
                        ));
                    }
                    if let Some(ref mut mapping) = row_ann_mapping {
                        // Check if this row variable name already has a mapping
                        if let Some(existing_var) = mapping.get(n) {
                            // Already mapped: return the existing RowVar with its current level
                            // from state.levels. DO NOT reset the level - unification may have
                            // lowered it, and level lowering must be monotone (Kiselyov 2013).
                            let current_level = *state.levels.get(existing_var).expect(
                                "invariant: row var registered in mapping must be in state.levels",
                            );
                            RowTail::RowVar(existing_var.clone(), current_level)
                        } else {
                            // First time seeing this row variable: create fresh var and register level
                            let fresh = format!("_t{}", state.name_counter);
                            state.name_counter += 1;
                            state.levels.insert(fresh.clone(), state.level);
                            mapping.insert(n.clone(), fresh.clone());
                            RowTail::RowVar(fresh, state.level)
                        }
                    } else {
                        // Outside of function scope, use the row variable name directly
                        // Use or_insert to atomically lookup-or-create, avoiding level reset
                        let level = *state.levels.entry(n.clone()).or_insert(state.level);
                        RowTail::RowVar(n.clone(), level)
                    }
                }
            };
            continue;
        }
        let key = match &entry.node.key {
            Some(k) => match &k.node {
                Expr::Str(s) => s.clone(),
                _ => {
                    return Err(TypeError::new(
                        "type record keys must be bare words",
                        k.span,
                    ))
                }
            },
            None => {
                return Err(TypeError::new(
                    "auto-indexed entries not supported in type expressions",
                    entry.span,
                ))
            }
        };
        let ty = resolve_type_expr(&entry.node.value, env, state, ann_mapping, row_ann_mapping)?;
        fields.insert(key, ty);
    }
    Ok(Type::Record(Row { fields, tail: rest }))
}

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
fn try_resolve_fn_type_expr(
    entries: &[Spanned<Entry>],
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

    let (ann_node, ann_span) = match &first.node.value.node {
        Expr::Annotated { name, annotation } if name == "Fn" => (&annotation.node, annotation.span),
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
            second.span,
        ));
    }

    let ret =
        resolve_annotation_as_type(ann_node, env, ann_span, state, ann_mapping, row_ann_mapping)?;

    // The parameter list can be:
    // - Expr::Dict(entries) — old/standard syntax: `[$a $b]` or `[$Number]`
    // - Expr::Call { implied: true, func, args } — new syntax: bare identifiers like `[a b]`
    //   parse as implied calls. Extract func + args as the parameter type expressions.
    let mut params = Vec::new();
    match &second.node.value.node {
        Expr::Dict(param_entries) => {
            for (pos, entry) in param_entries.iter().enumerate() {
                if let Some(ref key) = entry.node.key {
                    let key_name = match &key.node {
                        Expr::Str(s) => format!("'{s}'"),
                        _ => "unknown".to_string(),
                    };
                    return Err(TypeError::new(
                        format!(
                            "function type parameter at position {}: expected a type name, got key {key_name}",
                            pos + 1
                        ),
                        entry.span,
                    ));
                }
                params.push(resolve_type_expr(
                    &entry.node.value,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )?);
            }
        }
        Expr::Call {
            implied: true,
            func,
            args,
            ..
        } => {
            // New syntax: [TypeA TypeB] parses as an implied call.
            // Treat func as the first param, args as remaining params.
            params.push(resolve_type_expr(
                func,
                env,
                state,
                ann_mapping,
                row_ann_mapping,
            )?);
            for arg in args.iter() {
                params.push(resolve_type_expr(
                    arg,
                    env,
                    state,
                    ann_mapping,
                    row_ann_mapping,
                )?);
            }
        }
        _ => {
            return Err(TypeError::new(
                "function type parameter list must be a bracket expression",
                second.node.value.span,
            ))
        }
    }

    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
        variadic: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(input: &str) -> Result<(), Vec<TypeError>> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        typecheck_file(&file.node)
    }

    fn check_err(input: &str) -> Vec<TypeError> {
        check(input).unwrap_err()
    }

    fn infer(input: &str) -> Type {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        infer_expr(expr, &env, &mut state, &mut None).unwrap()
    }

    fn doc_env(input: &str) -> Rc<TypeEnv> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None).unwrap()
    }

    fn result_type(input: &str) -> Type {
        let env = doc_env(input);
        env.get("%").unwrap().body.clone()
    }

    fn result_field(input: &str, field: &str) -> Type {
        match result_type(input) {
            Type::Record(Row { fields, .. }) => fields.get(field).cloned().unwrap(),
            other => panic!("expected Record for %, got {other}"),
        }
    }

    fn file_env(input: &str) -> Rc<TypeEnv> {
        file_env_impl(input, false)
    }

    fn file_env_with_builtins(input: &str) -> Rc<TypeEnv> {
        file_env_impl(input, true)
    }

    fn file_env_impl(input: &str, with_builtins: bool) -> Rc<TypeEnv> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let mut env = if with_builtins {
            Rc::new(TypeEnv::with_builtins())
        } else {
            Rc::new(TypeEnv::new())
        };
        let mut state = InferState::new();
        let mut named_types: HashMap<String, Type> = HashMap::new();
        let mut pipeline_type = Type::Record(Row {
            fields: HashMap::new(),
            tail: RowTail::Empty,
        });
        for doc in &file.node.documents {
            match typecheck_document(
                doc,
                &env,
                &mut state,
                &mut None,
                &pipeline_type,
                &named_types,
            ) {
                Ok((new_env, doc_output_type, advisory)) => {
                    if !advisory.is_empty() {
                        panic!("file_env: advisory typecheck error: {:?}", advisory);
                    }
                    if let Some(ref name) = doc.node.name {
                        named_types.insert(name.clone(), doc_output_type.clone());
                    }
                    pipeline_type = doc_output_type;
                    env = new_env;
                }
                Err(errs) => panic!("file_env: typecheck error: {:?}", errs),
            }
        }
        env
    }

    // -- Literal inference --

    #[test]
    fn test_literal_int() {
        assert_eq!(infer("42"), Type::IntLiteral(42));
    }

    #[test]
    fn test_literal_float() {
        assert_eq!(infer("3.14"), Type::Float);
    }

    #[test]
    fn test_literal_bool() {
        assert_eq!(infer("true"), Type::Bool);
    }

    #[test]
    fn test_literal_string() {
        // In new syntax, bare words are references (VarRef), not string literals.
        // String literals require quotes.
        assert_eq!(infer("\"hello\""), Type::StringLiteral("hello".into()));
    }

    // -- VarRef --

    #[test]
    fn test_varref_in_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_varref_undefined() {
        let errors = check_err("$x");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("undefined variable: x"));
    }

    // -- Record construction --

    #[test]
    fn test_dict_simple() {
        // In new syntax, string values must be quoted.
        let ty = infer("[a: 1  b: \"hello\"  c: true]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
                assert_eq!(fields.get("b"), Some(&Type::StringLiteral("hello".into())));
                assert_eq!(fields.get("c"), Some(&Type::Bool));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_auto_indexed() {
        // In new syntax, bare words are references. For a data sequence of quoted strings,
        // use string literals. A quoted string in head position → Dict, so
        // ["foo" "bar" "baz"] is a Dict with auto-indexed entries.
        let ty = infer("[\"foo\" \"bar\" \"baz\"]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("0"), Some(&Type::StringLiteral("foo".into())));
                assert_eq!(fields.get("1"), Some(&Type::StringLiteral("bar".into())));
                assert_eq!(fields.get("2"), Some(&Type::StringLiteral("baz".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_nested() {
        let ty = infer("[outer: [inner: 42]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                let inner = fields.get("outer").unwrap();
                match inner {
                    Type::Record(Row {
                        fields: inner_fields,
                        ..
                    }) => {
                        assert_eq!(inner_fields.get("inner"), Some(&Type::IntLiteral(42)));
                    }
                    other => panic!("expected Record, got {other}"),
                }
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_ref() {
        let ty = infer("[a: $b  b: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // With let-generalization, forward references now participate in unification.
                // $b unifies with 42, so both a and b have type IntLiteral(42).
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // -- Dict error accumulation --

    #[test]
    fn test_dict_multiple_errors() {
        let errors = check_err("[a: $undefined1  b: 42  c: $undefined2]");
        assert_eq!(errors.len(), 2, "should return all errors, got: {errors:?}");
        assert!(
            errors[0].message.contains("undefined1"),
            "first error should be about undefined1, got: {}",
            errors[0].message
        );
        assert!(
            errors[1].message.contains("undefined2"),
            "second error should be about undefined2, got: {}",
            errors[1].message
        );

        // Also verify via direct infer_expr call
        let mut file = crate::parse("[a: $undefined1  b: 42  c: $undefined2]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        let errs = infer_expr(expr, &env, &mut state, &mut None).unwrap_err();
        assert_eq!(errs.len(), 2, "infer_expr should return all dict errors");
        assert!(errs[0].message.contains("undefined1"));
        assert!(errs[1].message.contains("undefined2"));
    }

    // -- Dot access --

    #[test]
    fn test_dot_access_found() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field(
                "[person: [name: \"Andrew\"  age: 30]]\n[result: $person.name]",
                "result"
            ),
            Type::StringLiteral("Andrew".into()),
        );
    }

    #[test]
    fn test_dot_access_missing_field() {
        // In new syntax, string literals require quotes.
        let errors = check_err("[person: [name: \"Andrew\"]]\n[result: $person.age]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("field 'age' not found")));
    }

    #[test]
    fn test_dot_access_non_record() {
        let errors = check_err("[x: 42]\n[result: $x.field]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected record type")));
    }

    // -- Access chain constraint generation (doc/07 Part 5) --

    #[test]
    fn test_dot_access_open_record_extends_tail() {
        // When `$p` has type Record({name: Str}, RowVar(ρ)) (an open record via type annotation)
        // and we access `$p.unknown` (field not in known fields), the RowVar case generates
        // constraint ρ → Row({unknown: β}, RowVar(ρ_fresh)) and returns β.
        // This extends the known tail rather than falling back to Any.
        //
        // Two accesses on the same open record accumulate constraints:
        // first access binds ρ → Row({f1: β₁}, RowVar(ρ₁))
        // second access sees ρ already bound, extracts from ρ₁ → Row({f2: β₂}, RowVar(ρ₂))
        // In new syntax, string literals require quotes.
        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");
        // r1 and r2 should both be TypeVars (fresh inferred types, not Any)
        match env.get("r1").map(|s| &s.body) {
            Some(Type::TypeVar(_, _)) => {}
            Some(other) => panic!("expected TypeVar for first unknown field, got {other}"),
            None => panic!("field 'r1' not found in env"),
        }
        match env.get("r2").map(|s| &s.body) {
            Some(Type::TypeVar(_, _)) => {}
            Some(other) => panic!("expected TypeVar for second unknown field, got {other}"),
            None => panic!("field 'r2' not found in env"),
        }
    }

    #[test]
    fn test_dot_access_constraint_generation_on_open_record_with_known_field() {
        // Task 5: Renamed from test_dot_access_open_record_infinite_row_cycle.
        // The original name promised "infinite row cycle" but the test actually exercises
        // TypeVar constraint generation on forward references, NOT the RowVar occurs-check path.
        //
        // Test the occurs-check error path in check_dot_access (typecheck.rs:710)
        //
        // ANALYSIS: The occurs check `if row_var_occurs_pub(rho, &binding, &state.subst)` fires
        // when binding ρ → Row({field: β}, RowVar(ρ_fresh)) would create an infinite row type.
        //
        // PROOF SKETCH (invariant at occurs-check site, typecheck.rs:710):
        //   For ρ to occur in the binding Row({field: β}, RowVar(ρ_fresh)), either:
        //     (a) β contains ρ in its structure (e.g., β is bound to Record(..., RowVar(ρ))), OR
        //     (b) ρ_fresh = ρ (the fresh row var equals the original)
        //
        //   Both are IMPOSSIBLE by construction:
        //     - β is fresh (line 696: state.fresh_type_var()) with no prior bindings → cannot contain ρ
        //     - ρ_fresh is fresh (line 698: state.fresh_row_var_name()) → ρ_fresh ≠ ρ by uniqueness
        //
        //   Therefore, row_var_occurs_pub(ρ, binding, state.subst) is ALWAYS false when the binding
        //   uses only fresh variables. The occurs check is defensive programming that guards the
        //   invariant but cannot fail under normal type inference.
        //
        // SIMILAR DEFENSIVE CHECKS: The unify_remainders occurs checks in types.rs CAN be triggered
        // because they deal with potentially non-fresh variables from both sides of a unification.
        // But check_dot_access creates fresh variables on-demand, making the cycle impossible.
        //
        // TEST STRATEGY: Pass 3b (row-unification-h) now unifies the two γ_data row bindings:
        //   - From check_dot_access: γ_data → Record({unknown: β}, RowVar(ρ))
        //   - From infer_dict for `data: [known: 1]`: γ_data → Record({known: 1}, Empty)
        //
        // Unifying an open constraint row with a closed concrete row where the constraint
        // field ("unknown") is absent from the concrete type is a type error — accessing
        // a non-existent field is correctly detected by Pass 3b unification.

        // Test: Accessing a non-existent field on a letrec forward-reference now produces
        // a type error via Pass 3b constraint unification. The constraint IS generated
        // (check_dot_access binds γ_data → Record({unknown: β}, RowVar(ρ)) in state.subst)
        // and then correctly checked against the concrete type of `data`.
        let result = check("[result: $data.unknown  data: [known: 1]]");
        assert!(
            result.is_err(),
            "Accessing non-existent field 'unknown' on a letrec forward reference should \
             produce a type error via Pass 3b constraint unification; got Ok"
        );

        // Note: The types.rs row occurs checks ARE tested (see test_row_occurs_check_direct_tail_cycle
        // and test_row_occurs_check_nested_in_field_cycle). Those tests demonstrate the occurs check
        // mechanism works correctly. The check_dot_access occurs check uses the same row_var_occurs_pub
        // function, so if it were ever triggered, it would work correctly.

        // CONCLUSION: This test documents that:
        // 1. The occurs check exists in check_dot_access (typecheck.rs)
        // 2. It uses row_var_occurs_pub which is tested in types.rs
        // 3. Constraint generation works correctly: γ_data → Record({unknown: β}, RowVar(ρ))
        // 4. Pass 3b now verifies constraints against concrete types, detecting field absence
    }

    #[test]
    fn test_dot_access_typevar_generates_constraint_verified() {
        // Task 6: Verifies that the constraint α = Record({name: β}, RowVar(ρ)) was generated
        // when dot-accessing a TypeVar target, and that β is now resolved via Pass 3b.
        //
        // WHAT WE'RE TESTING:
        //   [result: $data.name  data: [name: hello]]
        //
        //   During Pass 1 of infer_dict, each field gets a fresh TypeVar in dict_env.
        //   When Pass 3 processes `result: $data.name`, it calls infer_expr on `$data.name`.
        //   $data resolves to γ_data (the Pass 1 TypeVar for data). check_dot_access sees
        //   γ_data is a TypeVar and generates the constraint γ_data = Record({name: β}, RowVar(ρ))
        //   stored in state.subst, returning β as the type of `result`.
        //
        // HOW RESOLUTION NOW OCCURS (Pass 3b, row-unification-h):
        //   Pass 3b merges state.subst bindings into local subst after the loop.
        //   When γ_data appears in BOTH state.subst (→ Record({name: β}, RowVar(ρ))) and local
        //   subst (→ Record({name: StringLiteral("hello")}, Empty)), Pass 3b calls unify on them:
        //   unify(Record({name: StringLiteral("hello")}, Empty), Record({name: β}, RowVar(ρ)))
        //     → common field "name": unify(StringLiteral("hello"), β) → β → StringLiteral("hello")
        //     → ρ → Row({}, Empty) (tail unification)
        //   Pass 3c then applies subst to all field types: result's type β → StringLiteral("hello").
        //
        // ASSERTION:
        //   result's type is StringLiteral("hello") — the constraint was generated AND resolved
        //   by Pass 3b unification. Any would mean check_dot_access returned Any instead of
        //   generating the constraint.

        // In new syntax, string literals require quotes.
        let mut file = crate::parse("[result: $data.name  data: [name: \"hello\"]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        // Typecheck the document
        let doc_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .unwrap();

        // Get the type of 'result' — β, resolved by Pass 3b to StringLiteral("hello")
        let result_ty = match doc_env.get("result") {
            Some(scheme) => scheme.body.clone(),
            None => panic!("field 'result' not found"),
        };

        // ASSERTION: result's type must be a resolved concrete type, not Any and not TypeVar.
        // Any would mean check_dot_access fell through to the Any arm instead of generating
        // the constraint α = Record({name: β}, RowVar(ρ)).
        // TypeVar would mean Pass 3b failed to resolve β through the γ_data collision.
        // StringLiteral("hello") confirms constraint generation AND Pass 3b resolution.
        assert_eq!(
            result_ty,
            Type::StringLiteral("hello".to_string()),
            "result must be StringLiteral(\"hello\") — confirms constraint generation AND Pass 3b resolution; got {result_ty}"
        );
    }

    #[test]
    fn test_dot_access_open_record_extends_tail_distinct_vars() {
        // Task 4: Strengthen test_dot_access_open_record_extends_tail
        // Original test at line 1684 verifies r1 and r2 are TypeVars but not that they're DISTINCT.
        // This test adds the distinctness assertion.

        // In new syntax, string literals require quotes.
        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");

        // Get r1 and r2 types (should both be TypeVars)
        let r1_var_name = match env.get("r1").map(|s| &s.body) {
            Some(Type::TypeVar(name, _)) => name.clone(),
            Some(other) => panic!("expected TypeVar for r1, got {other}"),
            None => panic!("field 'r1' not found in env"),
        };

        let r2_var_name = match env.get("r2").map(|s| &s.body) {
            Some(Type::TypeVar(name, _)) => name.clone(),
            Some(other) => panic!("expected TypeVar for r2, got {other}"),
            None => panic!("field 'r2' not found in env"),
        };

        // Assert that r1 and r2 are DISTINCT TypeVars
        assert_ne!(
            r1_var_name, r2_var_name,
            "r1 and r2 should be distinct TypeVars (different field accesses should get fresh variables). \
             Got r1={}, r2={}",
            r1_var_name, r2_var_name
        );
    }

    #[test]
    fn test_typeassert_default_inference_error_propagation() {
        // Task 5: Test TypeAssert default inference-error propagation
        // resolve_type_assert at typecheck.rs:1102-1104 propagates Err(errs) when
        // the default expression itself fails to infer (e.g., references undefined variable).

        let errors = check_err("[@[type: Number  default: $undefined_var] 42]");

        // Should have at least one error (from the undefined variable in default)
        assert!(
            !errors.is_empty(),
            "TypeAssert with invalid default expression should produce an error"
        );

        // The error should mention the undefined variable
        assert!(
            errors.iter().any(|e| e.message.contains("undefined")),
            "Error should mention undefined variable, got: {:?}",
            errors
        );
    }

    // -- Bracket access --

    #[test]
    fn test_bracket_access_string_key() {
        // In new syntax, the bracket key must be a quoted string literal (not bare word).
        assert_eq!(
            result_field(
                "[data: [name: \"hello\"]]\n[result: $data[\"name\"]]",
                "result"
            ),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_bracket_access_int_key() {
        // In new syntax, data sequences of strings require quoted literals.
        assert_eq!(
            result_field("[list: [\"a\" \"b\" \"c\"]]\n[result: $list[0]]", "result"),
            Type::StringLiteral("a".into()),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_literal() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field(
                "[data: [x: 1]  key: \"x\"]\n[result: $data[$key]]",
                "result"
            ),
            Type::IntLiteral(1),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_non_literal() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field("[result: $data[$key]  data: [x: 1]  key: \"x\"]", "result"),
            Type::Any,
        );
    }

    // -- Range access --

    #[test]
    fn test_range_access_record() {
        let ty = result_field(
            "[data: [a: 1  b: 2  c: 3]]\n[result: $data[0..2]]",
            "result",
        );
        assert!(matches!(ty, Type::Record(..)));
    }

    #[test]
    fn test_range_access_invalid_bound() {
        let errors = check_err("[flag: true  data: [a: 1  b: 2]]\n[result: $data[$flag..2]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("range bound must be Int or String")));
    }

    // -- TypeAssert --

    #[test]
    fn test_type_assert_pass() {
        let ty = infer("[@Number 42]");
        assert_eq!(ty, Type::Number);
    }

    #[test]
    fn test_type_assert_fail() {
        // In new syntax, bare words are references. Use a quoted string to test type mismatch.
        let errors = check_err("[@Number \"hello\"]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("cannot unify"));
    }

    #[test]
    fn test_type_assert_int_not_string() {
        let errors = check_err("[@String 42]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("cannot unify"));
    }

    #[test]
    fn test_type_assert_default_suppresses_mismatch() {
        let result = check("[@[type: Number  default: 0] hello]");
        assert!(
            result.is_ok(),
            "TypeAssert with default: should not raise type error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_type_assert_no_default_still_errors() {
        // In new syntax, string literals require quotes. "hello" infers as Str, not Number.
        let errors = check_err("[@[type: Number] \"hello\"]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "TypeAssert without default: should still report type error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_wrong_type_emits_error() {
        // [@Number default: "hello" expr] — default is Str, asserted type is Number
        // Should emit a default value type mismatch error
        // In new syntax, string literals require quotes.
        let errors = check_err("[@[type: Number  default: \"hello\"] 42]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "TypeAssert with wrong default type should emit error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_correct_type_no_error() {
        // [@Number default: 0 expr] — default is IntLiteral(0) which is subtype of Number
        // Should not emit any error
        let result = check("[@[type: Number  default: 0] 42]");
        assert!(
            result.is_ok(),
            "TypeAssert with correct default type should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_wrong_type_main_check_fails() {
        // [@Number default: "hello" wrong_expr] — both main and default are wrong
        // Should emit a default value type mismatch error
        // In new syntax, string literals require quotes.
        let errors = check_err("[@[type: Number  default: \"hello\"] \"world\"]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "TypeAssert with wrong default and wrong expr should emit default mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_int_subtype_of_number() {
        // [@Number default: 42 expr] — IntLiteral(42) <: Number — no error
        let result = check("[@[type: Number  default: 42] hello]");
        assert!(
            result.is_ok(),
            "TypeAssert with Int default for Number assertion should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_string_literal_subtype_of_str() {
        // [@String default: "ok" expr] — StringLiteral("ok") <: Str — no error
        // In new syntax, string literals require quotes.
        let result = check("[@[type: String  default: \"ok\"] 42]");
        assert!(
            result.is_ok(),
            "TypeAssert with Str default for String assertion should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_suppresses_main_error_but_propagates_ok() {
        // Task 6: ASSERT-DEFAULT suppression — when a valid default is present, the
        // main-check error (hello is not a Number) is suppressed and typecheck returns Ok.
        //
        // resolve_type_assert (typecheck.rs) follows this logic:
        //   1. Infer main expr type; if mismatch AND default present → suppress, return Ok
        //   2. Infer default type; if default type mismatches asserted type → Err
        //
        // The expression is wrapped in a dict so the result is observable via result_field.
        // `hello` is a bare word (StringLiteral type), not a Number → mismatch, suppressed.
        let result = check("[result: [@[type: Number  default: 0] hello]]");
        assert!(
            result.is_ok(),
            "TypeAssert with valid default should suppress main-check error (hello is not a Number), \
             but typecheck returned: {:?}",
            result.unwrap_err()
        );
    }

    // -- TypeAlias --

    #[test]
    fn test_type_alias_record() {
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Person: [type [name: String  age: Number]]]\n[p: [@Person [name: \"Alice\"  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
                assert_eq!(fields.get("age"), Some(&Type::Number));
            }
            other => panic!("expected Record type from Person alias, got {other}"),
        }
    }

    #[test]
    fn test_type_alias_cycle_errors_not_loops() {
        // Circular aliases reference undefined types in each other. The aliases
        // themselves parse OK, but using them produces errors (not infinite loops).
        // The structure prevents cycles because aliases resolve against the parent
        // env, not the env being built.
        check("[A: [type B]  B: [type A]]").unwrap();
        let errors = check_err("[A: [type B]  B: [type A]  x: [@A 42]]");
        assert!(
            !errors.is_empty(),
            "using circular type aliases should produce errors"
        );
        let msg = format!("{:?}", errors);
        assert!(
            msg.contains("ndefined") || msg.contains("nknown"),
            "error should be about undefined/unknown type, got: {msg}"
        );
    }

    // -- Function inference --

    #[test]
    fn test_fn_unannotated() {
        let ty = infer("[fn [x] 42]");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Any]);
                assert_eq!(*ret, Type::IntLiteral(42));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_annotated_params() {
        let ty = infer("[fn [x@Number] $x]");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_match() {
        let ty = infer("[fn@Number [x@Number] $x]");
        match ty {
            Type::Function { ret, .. } => assert_eq!(*ret, Type::Number),
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_mismatch() {
        let errors = check_err("[fn@String [x@Number] $x]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("cannot unify"));
    }

    // -- Call --

    #[test]
    fn test_call_returns_function_ret_type() {
        assert_eq!(
            result_field("[f: [fn@Number [] 42]]\n[result: [call $f]]", "result"),
            Type::Number,
        );
    }

    #[test]
    fn test_call_non_function() {
        let errors = check_err("[x: 42]\n[result: [call $x]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected function type")));
    }

    #[test]
    fn test_check_call_with_scheme_non_function_scheme() {
        // Exercises the `_ => Err(not_a_function)` arm in check_call_with_scheme.
        //
        // check_call_with_scheme is only reached for polymorphic schemes (non-empty
        // type_vars or row_vars). The `_` arm fires when the instantiated body is
        // neither Type::Function nor Type::Any. We construct such a scheme directly:
        // ∀a. Int — polymorphic (has type_vars) but body is Int (not a function).
        // After instantiate_scheme, the body is still Int (no substitution to apply),
        // so the `_` arm fires and produces "expected function type".
        //
        // This guards the arm against removal or refactoring that would cause a panic
        // instead of a graceful error on malformed (but internally representable) schemes.
        let input = "[call $f 1]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        // Build env with `f: ∀a. Int` — polymorphic scheme, non-function body.
        // type_vars non-empty satisfies the dispatch guard at line ~286, routing to
        // check_call_with_scheme rather than check_call.
        let mut parent_env = TypeEnv::new();
        parent_env.insert_scheme(
            "f".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                row_vars: vec![],
                body: Type::Int,
            },
        );
        let parent_env = Rc::new(parent_env);

        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        let result = infer_expr(expr, &parent_env, &mut state, &mut None);

        // Must produce a not_a_function error, not a panic.
        assert!(
            result.is_err(),
            "calling a non-function polymorphic scheme should be an error"
        );
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expected function type")),
            "error should mention 'expected function type', got: {errors:?}"
        );
    }

    // -- Builtin sequence types --

    #[test]
    fn test_builtin_range_returns_seq_int() {
        // Regression test for type-seq sprint: $range should return Type::Seq(Int).
        // TypeEnv::with_builtins() registers range as Fn(Int, Int) -> Seq(Int).
        let input = "[result: [call $range 0 10]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();
        let new_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .expect("typecheck should succeed");

        let result_ty = new_env
            .get("result")
            .expect("result field should exist")
            .body
            .clone();

        assert_eq!(
            result_ty,
            Type::Seq(Box::new(Type::Int)),
            "range should return Seq(Int), got: {result_ty}"
        );
    }

    #[test]
    fn test_builtin_keys_returns_seq_str() {
        // Regression test for type-seq sprint: $keys should return Type::Seq(Str).
        // TypeEnv::with_builtins() registers keys as Fn(Record) -> Seq(Str).
        let input = "[d: [a: 1  b: 2]]\n[result: [call $keys $d]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let mut env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();

        // Process both documents
        for doc in &file.node.documents {
            env = typecheck_document_simple(doc, &env, &mut state, &mut None)
                .expect("typecheck should succeed");
        }

        let result_ty = env
            .get("result")
            .expect("result field should exist")
            .body
            .clone();

        assert_eq!(
            result_ty,
            Type::Seq(Box::new(Type::Str)),
            "keys should return Seq(Str), got: {result_ty}"
        );
    }

    #[test]
    fn test_builtin_plus_does_not_return_seq() {
        // Negative test: $+ returns Number, not Seq.
        // TypeEnv::with_builtins() registers + as Fn(Number, Number) -> Number.
        let input = "[result: [call $+ 1 2]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();
        let new_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .expect("typecheck should succeed");

        let result_ty = new_env
            .get("result")
            .expect("result field should exist")
            .body
            .clone();

        assert_eq!(
            result_ty,
            Type::Number,
            "+ should return Number, not Seq; got: {result_ty}"
        );

        // Explicitly verify it's NOT a Seq
        assert!(
            !matches!(result_ty, Type::Seq(_)),
            "+ should not return a Seq type"
        );
    }

    #[test]
    fn test_builtin_collect_returns_record_not_seq() {
        // $collect returns Record (open row), not Seq.
        // TypeEnv::with_builtins() registers collect as Fn(Seq(Any)) -> Record({...}).
        let input = "[s: [call $range 0 5]]\n[result: [call $collect $s]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let mut env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();

        // Process both documents
        for doc in &file.node.documents {
            env = typecheck_document_simple(doc, &env, &mut state, &mut None)
                .expect("typecheck should succeed");
        }

        let result_ty = env
            .get("result")
            .expect("result field should exist")
            .body
            .clone();

        // Should be a Record type (open row with RowVar tail)
        assert!(
            matches!(result_ty, Type::Record(_)),
            "collect should return Record, got: {result_ty}"
        );

        // Explicitly verify it's NOT a Seq
        assert!(
            !matches!(result_ty, Type::Seq(_)),
            "collect should not return a Seq type"
        );
    }

    // -- Document scope chain --

    #[test]
    fn test_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_intermediate_non_dict_error() {
        let errors = check_err("42\n[x: 1]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("expected record type"));
    }

    // -- % pipeline --

    #[test]
    fn test_pipeline_percent() {
        let env = file_env("[x: 42]\n---\n[y: %]");
        let result = env.get("%").unwrap().body.clone();
        match result {
            Type::Record(Row { fields, .. }) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert!(
                    matches!(y, Type::Record(..)),
                    "expected % to be Record, got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    #[test]
    fn test_pipeline_percent_type() {
        let env = file_env("[x: 1]\n---\n[y: %.x]");
        let result = env.get("%").unwrap().body.clone();
        match result {
            Type::Record(Row { fields, .. }) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert_eq!(
                    *y,
                    Type::IntLiteral(1),
                    "expected %.x to propagate IntLiteral(1), got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    // -- Annotation resolution --

    #[test]
    fn test_annotation_simple() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        assert_eq!(
            resolve_annotation(
                &Annotation::Simple("Int".into()),
                &env,
                span,
                &mut InferState::new(),
                &mut None,
                &mut None
            )
            .unwrap(),
            Type::Int,
        );
    }

    #[test]
    fn test_annotation_type_var() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        // InferState::new() has level=0, so annotation-derived TypeVars start at level 0
        // When no mapping is provided (outside function scope), a fresh var is created,
        // NOT the raw annotation name. This prevents cross-contamination between
        // two different `@a` annotations in the same dict.
        let mut state = InferState::new();
        let ty = resolve_annotation(
            &Annotation::Simple("a".into()),
            &env,
            span,
            &mut state,
            &mut None,
            &mut None,
        )
        .unwrap();
        // Should be a fresh TypeVar (not literally "a"), at level 0
        matches!(ty, Type::TypeVar(ref s, 0) if s.starts_with("_t"));
        // Counter should have advanced
        assert_eq!(state.name_counter, 1);
    }

    #[test]
    fn test_resolve_type_name_outside_function_scope() {
        // Test resolve_type_name None path (ann_mapping is None) when used outside function scope.
        // With Fix 1 applied: outside function scope, each call to resolve_type_name creates a
        // genuinely fresh type variable (not the raw annotation name).
        // This prevents two independent `[@a e1]` and `[@a e2]` annotations at top-level from
        // sharing the same substitution variable.
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        let mut state = InferState::new();

        // First call: creates fresh var (e.g. _t0)
        let ty1 = resolve_type_name("a", &env, span, &mut state, &mut None, &None).unwrap();
        // Second call: creates a DIFFERENT fresh var (e.g. _t1)
        let ty2 = resolve_type_name("a", &env, span, &mut state, &mut None, &None).unwrap();

        // Both should be TypeVars at level 0 but with different names
        match (&ty1, &ty2) {
            (Type::TypeVar(n1, 0), Type::TypeVar(n2, 0)) => {
                assert_ne!(
                    n1, n2,
                    "outside function scope, same annotation name must yield distinct fresh vars"
                );
                assert!(
                    n1.starts_with("_t"),
                    "fresh var should start with _t, got {n1}"
                );
                assert!(
                    n2.starts_with("_t"),
                    "fresh var should start with _t, got {n2}"
                );
            }
            other => panic!("expected two TypeVars at level 0, got: {other:?}"),
        }

        // Counter should have advanced twice
        assert_eq!(state.name_counter, 2);
    }

    #[test]
    fn test_resolve_type_name_outside_function_scope_monotonicity() {
        // With Fix 1: outside function scope each call gets a fresh var, so there is no
        // "second reference to the same annotation name" scenario — each use produces its
        // own fresh var. The monotonicity invariant (levels only decrease) still holds for
        // individual fresh vars; this test verifies the counter advances correctly.
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        let mut state = InferState::new();

        // Call at level 1
        state.level = 1;
        let ty1 = resolve_type_name("a", &env, span, &mut state, &mut None, &None).unwrap();

        // Call at level 2 (simulating a nested scope)
        state.level = 2;
        let ty2 = resolve_type_name("a", &env, span, &mut state, &mut None, &None).unwrap();

        // Each call produces a distinct TypeVar at its respective current level
        match (&ty1, &ty2) {
            (Type::TypeVar(n1, 1), Type::TypeVar(n2, 2)) => {
                assert_ne!(n1, n2, "distinct fresh vars for two outer-scope `@a` uses");
            }
            other => panic!("expected TypeVar(_t0, 1) and TypeVar(_t1, 2), got: {other:?}"),
        }
        // The old monotonicity test (second reference to same var) is now only relevant
        // inside function scope where mapping reuses the same fresh var. That path is tested
        // by test_annotation_level_monotonicity (within-function scope).
        assert_eq!(
            state.name_counter, 2,
            "counter must advance once per fresh var"
        );
    }

    #[test]
    fn test_ann_cross_kind_type_then_row_errors() {
        // Cross-kind collision: annotation name `a` used first as a type variable (@a on param x)
        // and then as a row variable (...a in the record annotation on param y).
        // resolve_type_dict detects this and emits a TypeError: "already used as a type variable".
        //
        // The cross-kind check is in the type→row direction: when a name that is already in
        // ann_mapping (TypeVar) is encountered as a row-variable tail in row_ann_mapping.
        let result = check("[fn [x@a y@[name: Int ...a]] $x]");
        assert!(
            result.is_err(),
            "cross-kind annotation collision (TypeVar then RowVar) must produce a TypeError"
        );
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("already used as a type variable")),
            "cross-kind collision must produce descriptive error; got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    // === Unit tests for the three type system fixes ===

    // --- Fix 1: outer-scope annotation names create fresh vars ---

    #[test]
    fn test_fix1_outer_scope_annotations_are_independent() {
        // Two TypeAssert annotations at the top level both using `@a`.
        // Before Fix 1, they shared TypeVar("a"): after resolving `[@a 42]` bound "a" to
        // IntLiteral(42), the second `[@a "hello"]` would fail with "cannot unify Int with String"
        // (cross-contamination). After Fix 1, each gets its OWN fresh TypeVar, so each fails
        // only for its own reason (TypeVar expected type can't satisfy a concrete literal in
        // check_expr's is_subtype path) — NOT because of interference from the sibling.
        //
        // The key invariant: if there ARE errors, they must NOT be a "cannot unify Int with String"
        // or similar cross-type error caused by one entry contaminating the other.
        let errors = check_err("[x: [@a 42]  y: [@a hello]]");
        // Neither error should mention Int/String cross-contamination
        let has_cross_contamination = errors.iter().any(|e| {
            (e.message.contains("Int") || e.message.contains("Number"))
                && (e.message.contains("String") || e.message.contains("hello"))
        });
        assert!(
            !has_cross_contamination,
            "errors must not be caused by cross-contamination between sibling @a annotations; \
             got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fix1_outer_scope_annotation_does_not_contaminate_siblings() {
        // Concrete types in outer-scope TypeAssert shouldn't be affected by Fix 1 —
        // concrete type names (Number, Int, String) are resolved as concrete types, not
        // fresh TypeVars. Only lowercase annotation names get fresh vars.
        // Verify that concrete-type annotations still work correctly at the top level.
        // In new syntax, string literals require quotes.
        let result = check("[x: [@Number 42]  y: [@String \"hello\"]]");
        assert!(
            result.is_ok(),
            "concrete-type annotations at top level should work (not affected by Fix 1): {:?}",
            result.unwrap_err()
        );
    }

    // --- Fix 2: cross-kind collision row→type direction ---

    #[test]
    fn test_fix2_cross_kind_row_then_type_errors() {
        // Cross-kind collision: annotation name `r` used first as a row variable (`...r`
        // in a record type annotation on param x), then as a type variable (`@r` on param y).
        // resolve_type_name must detect that `r` is already in row_ann_mapping and reject.
        let result = check("[fn [x@[name: Int ...r] y@r] $x]");
        assert!(
            result.is_err(),
            "cross-kind annotation collision (RowVar then TypeVar) must produce a TypeError"
        );
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("already used as a row variable")),
            "cross-kind collision (row→type direction) must produce descriptive error; got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fix2_cross_kind_type_then_row_still_errors() {
        // Existing behavior: type→row direction collision still produces an error
        // (regression guard for the pre-Fix-2 behavior that must be preserved).
        let result = check("[fn [x@a y@[name: Int ...a]] $x]");
        assert!(
            result.is_err(),
            "cross-kind collision (TypeVar then RowVar) must still produce a TypeError"
        );
    }

    // --- Fix 3: TypeAssert default type validation ---

    #[test]
    fn test_fix3_default_wrong_type_emits_error() {
        // The main expression (42) satisfies the assertion (Number), but the default
        // value ("hello") does NOT — it's a String. This should be a type error.
        // In new syntax, string literals require quotes.
        let errors = check_err("[@[type: Number  default: \"hello\"] 42]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "default with wrong type must emit 'default value type mismatch' error; got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fix3_default_correct_type_no_error() {
        // Main expression (hello as VarRef → undefined) does NOT satisfy Number, but default (0) DOES.
        // The type error for the main expression is suppressed, and the default is valid.
        // No error should be emitted (TypeAssert default suppression applies to undefined vars too).
        let result = check("[@[type: Number  default: 0] hello]");
        assert!(
            result.is_ok(),
            "TypeAssert with correct default type should not emit an error; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_fix3_default_wrong_type_main_also_wrong_emits_error() {
        // Both the main expression (world) and the default (hello) fail the Number assertion.
        // The type error for the main expression would be suppressed (default present),
        // but the default itself is wrong — must emit a 'default value type mismatch' error.
        // In new syntax, string literals require quotes.
        let errors = check_err("[@[type: Number  default: \"hello\"] \"world\"]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "default with wrong type must emit error even when main also fails; got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_annotation_property_dict_with_type() {
        let ty = infer("[fn [x@[type: Number  default: 0]] $x]");
        match ty {
            Type::Function { params, .. } => assert_eq!(params, vec![Type::Number]),
            other => panic!("expected Function, got {other}"),
        }
    }

    // -- resolve_property_dict_as_record fallback paths --

    #[test]
    fn test_property_dict_non_str_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Int(42))),
                value: Rc::new(sp(Expr::Str("Int".into()))),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(
                &ann,
                &env,
                span,
                &mut InferState::new(),
                &mut None,
                &mut None
            )
            .unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_no_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: Rc::new(sp(Expr::Str("Int".into()))),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(
                &ann,
                &env,
                span,
                &mut InferState::new(),
                &mut None,
                &mut None
            )
            .unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_unresolvable_type_propagates_error() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: Rc::new(sp(Expr::Str("NoSuchType".into()))),
            },
            span,
        )]);
        let result = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("undefined type: NoSuchType"));
    }

    #[test]
    fn test_property_dict_literal_value_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: Rc::new(sp(Expr::Int(30))),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(
                &ann,
                &env,
                span,
                &mut InferState::new(),
                &mut None,
                &mut None
            )
            .unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_fn_type_error_propagates() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        // [Fn@Int] -- function type pattern detected (Fn@ prefix) but wrong
        // number of entries: should propagate, not fall back to Any.
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: Rc::new(sp(Expr::Annotated {
                    name: "Fn".into(),
                    annotation: Spanned::new(Annotation::Simple("Int".into()), span),
                })),
            },
            span,
        )]);
        let result = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("function type"));
    }

    // -- Type alias in scope --

    #[test]
    fn test_type_alias_in_scope_chain() {
        let ty = result_field(
            "[Coord: [type [x: Number  y: Number]]]\n[p: [@Coord [x: 1  y: 2]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("x"), Some(&Type::Number));
                assert_eq!(fields.get("y"), Some(&Type::Number));
                assert_eq!(fields.len(), 2);
            }
            other => panic!("expected Record type from Coord alias, got {other}"),
        }
    }

    #[test]
    fn test_type_alias_shadowing_allows_nested_redefinition() {
        // Inner dict can shadow outer dict's type alias — lexical scoping
        // Type aliases are excluded from the record's fields, so we test via usage
        let ty = result_field(
            "[ID: [type Int]  outer: [@ID 42]  nested: [ID: [type String]  inner: [@ID \"text\"]]]",
            "nested",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                // nested.ID is a type alias, so it's NOT in fields (type aliases excluded from record)
                assert_eq!(fields.get("ID"), None);
                // nested.inner uses the shadowed String type (not the outer Int type)
                assert_eq!(fields.get("inner"), Some(&Type::Str));
            }
            other => panic!("expected Record type, got {other}"),
        }
    }

    // -- Error branch coverage --

    #[test]
    fn test_type_expr_non_bare_word_key() {
        let errors = check_err("[type [$var: Int]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("type record keys must be bare words")));
    }

    #[test]
    fn test_type_expr_auto_indexed_entries() {
        // In new syntax, [Int String] is an implied call (Identifier head), not a data
        // sequence. The type resolver sees an invalid type expression (not a dict).
        // In old syntax this was a Dict with auto-indexed Str entries → different error.
        // For testing the auto-indexed rejection, use quoted strings: ["Int" "String"].
        let errors = check_err("[type [\"Int\" \"String\"]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("auto-indexed entries not supported")));
    }

    #[test]
    fn test_annotation_type_value_invalid_expr() {
        let errors = check_err("[fn [x@[type: 42]] $x]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("invalid type expression")));
    }

    #[test]
    fn test_annotation_composite_function_type() {
        let ty =
            infer("[fn [f@[type: [Fn@Number [Int]] default: [fn [x] $x]]] [@Number [call $f 42]]]");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1);
                match &params[0] {
                    Type::Function {
                        params: inner_params,
                        ret: inner_ret,
                        variadic: _,
                    } => {
                        assert_eq!(*inner_params, vec![Type::Int]);
                        assert_eq!(**inner_ret, Type::Number);
                    }
                    other => panic!("expected Function param, got {other}"),
                }
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_annotation_composite_record_type() {
        let ty = infer(
            "[fn [p@[type: [name: String  age: Number] default: [name: Alice  age: 30]]] $p.name]",
        );
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1);
                match &params[0] {
                    Type::Record(row) => {
                        assert_eq!(row.fields.get("name"), Some(&Type::Str));
                        assert_eq!(row.fields.get("age"), Some(&Type::Number));
                    }
                    other => panic!("expected Record param, got {other}"),
                }
                assert_eq!(*ret, Type::Str);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_annotation_composite_type_in_type_assert() {
        let ty =
            infer("[f: [fn [x] $x]  result: [@[type: [Fn@Number [Int]] default: [fn [x] 0]] $f]]");
        let result_ty = match ty {
            Type::Record(row) => row.fields.get("result").cloned(),
            other => panic!("expected Record, got {other}"),
        };
        match result_ty {
            Some(Type::Function {
                params,
                ret,
                variadic: _,
            }) => {
                assert_eq!(params, vec![Type::Int]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function for result field, got {other:?}"),
        }
    }

    #[test]
    fn test_annotation_nested_composite_higher_order_function() {
        // Nested composite type: [type: [Fn@[Fn@Int [Int]] [Int]]]
        // Resolves to Fn(Int -> Fn(Int -> Int)) — a curried function.
        // Exercises recursive resolve_type_expr: the return type [Fn@Int [Int]] is
        // itself a Fn type expression that must be recursively resolved.
        let ty = infer(
            "[fn [f@[type: [Fn@[Fn@Int [Int]] [Int]] default: [fn [x] [fn [y] $y]]]] [call $f 0]]",
        );
        // f has type Fn(Int -> Fn(Int -> Int))
        // [call $f 0] has return type Fn(Int -> Int)
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1);
                // param type: Fn(Int -> Fn(Int -> Int))
                match &params[0] {
                    Type::Function {
                        params: outer_params,
                        ret: outer_ret,
                        variadic: _,
                    } => {
                        assert_eq!(*outer_params, vec![Type::Int]);
                        // return type: Fn(Int -> Int)
                        match outer_ret.as_ref() {
                            Type::Function {
                                params: inner_params,
                                ret: inner_ret,
                                variadic: _,
                            } => {
                                assert_eq!(*inner_params, vec![Type::Int]);
                                assert_eq!(**inner_ret, Type::Int);
                            }
                            other => panic!("expected Fn(Int -> Int) as outer return, got {other}"),
                        }
                    }
                    other => panic!("expected Fn(Int -> Fn(Int -> Int)) param, got {other}"),
                }
                // [call $f 0] return type: Fn(Int -> Int)
                match ret.as_ref() {
                    Type::Function {
                        params: ret_params,
                        ret: ret_ret,
                        variadic: _,
                    } => {
                        assert_eq!(*ret_params, vec![Type::Int]);
                        assert_eq!(**ret_ret, Type::Int);
                    }
                    other => panic!("expected Fn(Int -> Int) return, got {other}"),
                }
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_non_dict_record_open_row_scheme_preservation() {
        // Row polymorphism in non-Dict Record scheme preservation.
        // The make-record function returns a record containing a function `project`
        // that is polymorphic over the row tail (open record annotation `...`).
        // When [call $make-record] appears at a document boundary, typecheck_document
        // must preserve the row-polymorphic scheme for `project`, not monomorphize it.
        //
        // project: [fn [r@[x: Int ...]] $r.x] has type ∀ρ. Fn(Record{x: Int | ρ} → Int)
        // It can be called with different record shapes (open row tail).
        let input = r#"
            [make-record: [fn [] [project: [fn [r@[x: Int ...]] $r.x]]]]
            ---
            [call $make-record]
            ---
            [r1: [call $project [x: 1  y: "hello"]]
             r2: [call $project [x: 2  z: true]]]
        "#;
        // Both r1 and r2 should typecheck successfully with different extra fields.
        // If project were monomorphized, the row tail would be fixed and the second
        // call with different extra fields might fail.
        check(input).expect("row-polymorphic non-Dict Record scheme should be preserved");

        // Additionally verify the scheme is not monomorphized by calling doc_env
        // on just the first two documents and checking the `project` scheme.
        let two_doc_input = r#"
            [make-record: [fn [] [project: [fn [r@[x: Int ...]] $r.x]]]]
            ---
            [call $make-record]
        "#;
        let env = {
            let mut file = crate::parse(two_doc_input).unwrap();
            crate::desugar::desugar_file(&mut file.node);
            let mut env = Rc::new(TypeEnv::new());
            let mut state = InferState::new();
            for doc in &file.node.documents {
                env = typecheck_document_simple(doc, &env, &mut state, &mut None).unwrap();
            }
            env
        };
        // project should be in scope as a polymorphic scheme (has_bound_vars)
        let project_scheme = env
            .get("project")
            .expect("project should be threaded into env from non-Dict Record");
        assert!(
            !project_scheme.type_vars.is_empty() || !project_scheme.row_vars.is_empty(),
            "project scheme should be polymorphic (open row tail), got monomorphic: {:?}",
            project_scheme
        );
    }

    #[test]
    fn test_bracket_access_bool_key() {
        let errors = check_err("[data: [x: 1]  flag: true]\n[result: $data[$flag]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("bracket access key must be String or Int")));
    }

    #[test]
    fn test_annotated_non_fn_resolves_annotation() {
        let ty = infer("Config@Number");
        assert_eq!(ty, Type::Number);
    }

    // -- Fn@Return [Params] type expression --

    #[test]
    fn test_fn_type_one_param() {
        let ty = result_field(
            "[Mapper: [type [Fn@b [a]]]]\n[x: [@Mapper [fn [v] $v]]]",
            "x",
        );
        match ty {
            // After Fix 1: type alias annotation names become fresh internal vars.
            // The param type and return type should both be TypeVars (different ones).
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "expected 1 param");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "param should be a TypeVar, got {:?}",
                    params[0]
                );
                assert!(
                    matches!(*ret, Type::TypeVar(_, _)),
                    "ret should be a TypeVar, got {ret:?}"
                );
                // The param var and return var must be DIFFERENT (a ≠ b in [Fn@b [a]])
                assert_ne!(
                    params[0], *ret,
                    "param and return TypeVars must be distinct"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_two_params() {
        let ty = result_field(
            "[BinOp: [type [Fn@c [a b]]]]\n[x: [@BinOp [fn [p q] $p]]]",
            "x",
        );
        match ty {
            // After Fix 1: annotation names become fresh internal vars.
            // Two distinct names (a, b, c) → three distinct TypeVars.
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2, "expected 2 params");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "params[0] should be TypeVar, got {:?}",
                    params[0]
                );
                assert!(
                    matches!(params[1], Type::TypeVar(_, _)),
                    "params[1] should be TypeVar, got {:?}",
                    params[1]
                );
                assert!(
                    matches!(*ret, Type::TypeVar(_, _)),
                    "ret should be TypeVar, got {ret:?}"
                );
                // All three annotation names (a, b, c) are distinct → three distinct TypeVars
                assert_ne!(params[0], params[1], "params[0] and params[1] must differ");
                assert_ne!(params[1], *ret, "params[1] and ret must differ");
                assert_ne!(params[0], *ret, "params[0] and ret must differ");
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_types() {
        let ty = result_field(
            "[Add: [type [Fn@Number [Number Number]]]]\n[x: [@Add [fn [a@Number b@Number] $a]]]",
            "x",
        );
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Number, Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_return_typevar_param() {
        let ty = result_field(
            "[Pred: [type [Fn@Bool [a]]]]\n[x: [@Pred [fn [v] true]]]",
            "x",
        );
        match ty {
            // After Fix 1: annotation name `a` becomes a fresh internal var.
            // Return type is concrete Bool (not affected).
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "expected 1 param");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "param should be a TypeVar, got {:?}",
                    params[0]
                );
                assert_eq!(*ret, Type::Bool);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_higher_order() {
        let ty = result_field(
            "[HO: [type [Fn@[Fn@c [b]] [a]]]]\n[x: [@HO [fn [v] [fn [w] $w]]]]",
            "x",
        );
        match ty {
            // After Fix 1: annotation names a, b, c become fresh internal vars.
            // The outer function: param is TypeVar (a), return is inner Function.
            // The inner function: param is TypeVar (b), return is TypeVar (c).
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "outer should have 1 param");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "outer param should be TypeVar, got {:?}",
                    params[0]
                );
                match *ret {
                    Type::Function {
                        params: inner_params,
                        ret: inner_ret,
                        variadic: _,
                    } => {
                        assert_eq!(inner_params.len(), 1, "inner should have 1 param");
                        assert!(
                            matches!(inner_params[0], Type::TypeVar(_, _)),
                            "inner param should be TypeVar, got {:?}",
                            inner_params[0]
                        );
                        assert!(
                            matches!(*inner_ret, Type::TypeVar(_, _)),
                            "inner ret should be TypeVar, got {inner_ret:?}"
                        );
                        // All three annotation names (a, b, c) are distinct
                        assert_ne!(params[0], inner_params[0], "outer param != inner param");
                        assert_ne!(inner_params[0], *inner_ret, "inner param != inner ret");
                    }
                    other => panic!("expected inner Function, got {other}"),
                }
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_missing_param_list() {
        let errors = check_err("[type [Fn@b]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_extra_entries() {
        let errors = check_err("[type [Fn@b [a] extra]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_param_list_not_bracket() {
        let errors = check_err("[type [Fn@b a]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("parameter list must be a bracket expression")));
    }

    #[test]
    fn test_fn_type_standalone_fn_annotation() {
        let ty = infer("Fn@Number");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert!(params.is_empty());
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_in_type_assert() {
        let ty = result_field(
            "[F: [type [Fn@Number [Number]]]]\n[x: [@F [fn [n@Number] $n]]]",
            "x",
        );
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_display_round_trip() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("c".into(), 0)),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@c [a b]");
    }

    // -- Polymorphic call unification --

    #[test]
    fn test_call_polymorphic_identity() {
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result"),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_identity_string() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field(
                "[id: [fn [x@a] $x]]\n[result: [call $id \"hello\"]]",
                "result"
            ),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_two_type_vars() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field(
                "[f: [fn [x@a y@b] $y]]\n[result: [call $f 42 \"hello\"]]",
                "result"
            ),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_type_var_in_return_only() {
        // In new syntax, string literals require quotes (hello is unused in result).
        assert_eq!(
            result_field(
                "[first: [fn [x@a y@b] $x]]\n[result: [call $first 42 \"hello\"]]",
                "result"
            ),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_multiple_calls_different_types() {
        // In new syntax, string literals require quotes.
        let ty = result_type("[id: [fn [x@a] $x]]\n[r1: [call $id 42]  r2: [call $id \"hello\"]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("r1"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("r2"), Some(&Type::StringLiteral("hello".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_call_monomorphic_no_unification() {
        assert_eq!(
            result_field(
                "[f: [fn@Number [x@Number] $x]]\n[result: [call $f 42]]",
                "result"
            ),
            Type::Number,
        );
    }

    #[test]
    fn test_call_polymorphic_arity_mismatch_error() {
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_monomorphic_arity_mismatch() {
        let errors = check_err("[f: [fn@Number [x@Number y@Number] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch for monomorphic function, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_unification_error() {
        // In new syntax, string literals require quotes. Both args must unify to same type.
        let errors = check_err("[f: [fn [x@a y@a] $x]]\n[result: [call $f 42 \"hello\"]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "expected unification error, got: {:?}",
            errors
        );
    }

    // -- Polymorphic call with named args --

    #[test]
    fn test_call_polymorphic_with_named_arg() {
        // Polymorphic function called with only named args (no positional args).
        // The function has 1 param; 1 named arg fills it → total_supplied = 1 = params.len() → ok.
        // Named arg types are inferred for LSP hover but not unified with param types
        // (TODO(named-arg-types): requires param names in Type::Function).
        let result = check(
            "[f: [fn [x@a] $x]]
             ---
             [result: [call $f x: 42]]",
        );
        assert!(
            result.is_ok(),
            "call with 1 named arg filling 1 param slot should not produce arity error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_call_polymorphic_positional_plus_named_arity_error() {
        // Polymorphic function with 2 params called with 2 positional args AND 1 named arg.
        // total_supplied = 3 != params.len() = 2 → arity error.
        // At runtime this would also fail (C-NO-OVERLAP: named arg targets a positionally-bound param).
        let errors = check_err(
            "[f: [fn [x@a y@b] $x]]
             ---
             [result: [call $f 42 hello y: 77]]",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch for 2 positional + 1 named against 2 params, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_polymorphic_named_arg_bad_value_errors() {
        // A named arg whose value references an undefined variable should produce
        // a type error even in a polymorphic call context.
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42 hello y: $missing]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("undefined variable")),
            "expected undefined variable error from named arg, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_polymorphic_positional_plus_named_arity_ok() {
        // Polymorphic function with 2 params called with 1 positional arg + 1 named arg.
        // total_supplied = args.len() + named_args.len() = 1 + 1 = 2 = params.len() → ok.
        // This is a regression test for the named arg arity counting fix.
        let result = check(
            "[f: [fn [a b] $a]]
             ---
             [result: [call $f 1 b: 2]]",
        );
        result.expect(
            "call with 1 positional + 1 named arg filling 2 param slots should not produce arity error",
        );
        let env = file_env(
            "[f: [fn [a b] $a]]
             ---
             [result: [call $f 1 b: 2]]",
        );
        let result_ty = env.get("result").expect("result should be in env");
        assert!(
            !matches!(&result_ty.body, Type::Error),
            "result type should not be Type::Error, got: {:?}",
            result_ty.body
        );
    }

    // -- Function type expression with param list --

    #[test]
    fn test_fn_type_expr_with_params() {
        // [Identity: [type [Fn@a [a]]]] — identity-function type: param and return are SAME TypeVar.
        // After Fix 1: annotation names in type aliases become fresh internal vars, but within one
        // alias expression the same name (here `a`) maps to the SAME fresh var.
        let env = doc_env("[Identity: [type [Fn@a [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Identity");
        assert!(alias.is_some(), "Identity alias should be registered");
        match alias.unwrap() {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "Identity should have 1 param");
                // The param and return must be the SAME TypeVar (both reference annotation `a`)
                assert_eq!(
                    params[0], **ret,
                    "Identity function: param and return must be the same TypeVar (both use `a`)"
                );
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "param should be TypeVar, got {:?}",
                    params[0]
                );
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_multi_params() {
        // [Mapper: [type [Fn@b [a b]]]] — map function type: params[0]=a, params[1]=b, ret=b.
        // After Fix 1: fresh internal vars, but `b` in params[1] and `b` in ret must be the SAME
        // TypeVar (same mapping within the alias scope). `a` must be a DIFFERENT TypeVar from `b`.
        let env = doc_env("[Mapper: [type [Fn@b [a b]]]]\n[x: 1]");
        let alias = env.get_type_alias("Mapper").unwrap();
        match alias {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2, "Mapper should have 2 params");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "params[0] (a) should be TypeVar"
                );
                assert!(
                    matches!(params[1], Type::TypeVar(_, _)),
                    "params[1] (b) should be TypeVar"
                );
                // params[1] and ret both reference annotation `b`, so they must be equal
                assert_eq!(
                    params[1], **ret,
                    "params[1] and ret must be the same TypeVar (both use `b`)"
                );
                // params[0] (a) and params[1] (b) must be distinct
                assert_ne!(
                    params[0], params[1],
                    "params[0] (a) and params[1] (b) must differ"
                );
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_concrete_params() {
        let env = doc_env("[Add: [type [Fn@Number [Number Number]]]]\n[x: 1]");
        let alias = env.get_type_alias("Add").unwrap();
        match alias {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, &vec![Type::Number, Type::Number]);
                assert_eq!(**ret, Type::Number);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_predicate() {
        // [Pred: [type [Fn@Bool [a]]]] — predicate type: param is TypeVar (a), return is Bool.
        // After Fix 1: annotation name `a` becomes a fresh internal var. Bool is unchanged.
        let env = doc_env("[Pred: [type [Fn@Bool [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Pred").unwrap();
        match alias {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "Pred should have 1 param");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "param (a) should be TypeVar, got {:?}",
                    params[0]
                );
                assert_eq!(**ret, Type::Bool);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    // -- Row polymorphism --

    #[test]
    fn test_type_expr_open_record() {
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::RowVar(name, _),
            }) if name.starts_with("_open") => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
            }
            other => panic!("expected open Record with fresh row var, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_row_var_record() {
        // [WithName: [type [name: String ...rest]]] — record type with a named row variable.
        // After Fix 1: row variable names in type aliases become fresh internal vars (e.g., _t1)
        // rather than keeping the user-visible name "rest". The structural shape must still be
        // correct: a Record with a RowVar tail.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[WithName: [type [name: String ...rest]]]\n[p: [@WithName [name: \"Alice\"]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::RowVar(name, _),
            }) => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
                // The internal name is a fresh var — check it starts with the fresh-var prefix
                assert!(
                    name.starts_with("_t") || name == "rest",
                    "row var name should be a fresh internal var, got: {name}"
                );
            }
            other => panic!("expected record with row var, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_closed_record() {
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Closed: [type [name: String]]]\n[p: [@Closed [name: \"Alice\"]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                tail: RowTail::Empty,
                ..
            }) => {}
            other => panic!("expected closed Record, got {other}"),
        }
    }

    #[test]
    fn test_anonymous_open_record_annotations_get_fresh_vars() {
        // Each anonymous open record annotation should get a distinct row variable
        // Use inline open record annotations in function parameters
        let code = r#"
            [f: [fn [x@[a: Int ...]  y@[b: String ...]]
                 [x: $x  y: $y]]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with distinct row vars: {:?}",
            result
        );

        // Verify the inferred type has distinct row variables for x and y
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                // Extract the row variable names from both parameters
                let (row_var_x, row_var_y) = match (&params[0], &params[1]) {
                    (
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_x, _),
                            ..
                        }),
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_y, _),
                            ..
                        }),
                    ) => (name_x, name_y),
                    other => panic!("expected both params to be open records, got {:?}", other),
                };

                // The row variables should be distinct (different fresh names were generated)
                assert_ne!(
                    row_var_x, row_var_y,
                    "anonymous open record annotations must generate distinct row variables"
                );

                // Both should start with "_open" prefix
                assert!(
                    row_var_x.starts_with("_open"),
                    "expected row var name to start with _open, got {}",
                    row_var_x
                );
                assert!(
                    row_var_y.starts_with("_open"),
                    "expected row var name to start with _open, got {}",
                    row_var_y
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_cross_function_anonymous_open_records_get_fresh_vars() {
        // Two separate functions using anonymous open record annotations
        // should NOT share the same row variable (the bug that fresh name generation fixed)
        // Each function gets its own independent open record constraint
        let code = r#"
            [f: [fn [x@[a: Int ...]] $x.a]
             g: [fn [y@[b: String ...]] $y.b]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with independent open record constraints: {:?}",
            result
        );

        // Verify that f and g have distinct row variables
        let ty_f = result_field(code, "f");
        let ty_g = result_field(code, "g");

        let row_var_f = match ty_f {
            Type::Function { params, .. } => match &params[0] {
                Type::Record(Row {
                    tail: RowTail::RowVar(name, _),
                    ..
                }) => name.clone(),
                other => panic!("expected f param to be open record, got {:?}", other),
            },
            other => panic!("expected f to be function type, got {other}"),
        };

        let row_var_g = match ty_g {
            Type::Function { params, .. } => match &params[0] {
                Type::Record(Row {
                    tail: RowTail::RowVar(name, _),
                    ..
                }) => name.clone(),
                other => panic!("expected g param to be open record, got {:?}", other),
            },
            other => panic!("expected g to be function type, got {other}"),
        };

        // The two functions must have distinct row variables
        // If they incorrectly shared RowVar("_open", 0), this assertion would fail
        assert_ne!(
            row_var_f, row_var_g,
            "cross-function anonymous open records must generate distinct row variables"
        );

        // Both should be fresh _open names
        assert!(
            row_var_f.starts_with("_open"),
            "expected row var name to start with _open, got {}",
            row_var_f
        );
        assert!(
            row_var_g.starts_with("_open"),
            "expected row var name to start with _open, got {}",
            row_var_g
        );
    }

    #[test]
    fn test_named_row_var_level_monotonicity() {
        // Test for the row variable level monotonicity bug fix (computer-scientist-c31).
        // When a function has multiple parameters sharing a named row variable tail
        // (e.g., [fn [x@[a: Int ...r] y@[b: Str ...r]] body]), the second reference
        // to ...r should NOT reset r's level in state.levels. This mirrors the
        // resolve_type_name monotonicity fix from C71.
        //
        // Before the fix, state.levels.insert(fresh_name.clone(), state.level) ran
        // unconditionally on every ...r reference, resetting the level even if
        // unification had lowered it. After the fix, we check if the row variable
        // is already mapped and preserve its current level from state.levels.
        let code = r#"
            [f: [fn [x@[a: Int ...r]  y@[b: String ...r]]
                 [x: $x  y: $y]]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with shared named row variable: {:?}",
            result
        );

        // Verify both parameters share the same row variable
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                let (row_var_x, row_var_y) = match (&params[0], &params[1]) {
                    (
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_x, _),
                            ..
                        }),
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_y, _),
                            ..
                        }),
                    ) => (name_x, name_y),
                    other => panic!("expected both params to be open records, got {:?}", other),
                };

                // Both parameters should share the same row variable name
                // (both map to the same fresh variable _tN through the ann_mapping)
                assert_eq!(
                    row_var_x, row_var_y,
                    "named row variable ...r should be shared between parameters"
                );

                // The shared row variable should have a fresh name (from ann_mapping)
                assert!(
                    row_var_x.starts_with("_t"),
                    "expected fresh row var name to start with _t, got {}",
                    row_var_x
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_check_dot_access_lowers_row_var_levels() {
        // Test that check_dot_access correctly lowers inner type variable levels when
        // accessing an unknown field on an open record with a RowVar tail.
        //
        // SCENARIO:
        //   When check_dot_access handles a RowVar tail (typecheck.rs:695-761), it creates
        //   fresh type var β and fresh row var ρ_fresh at the current state.level, then
        //   calls lower_row_var_levels_pub(&binding, rho_level, state) at line 755.
        //
        //   ρ must be at an OUTER level (lower number) than β and ρ_fresh for the lowering
        //   to be non-trivial. We use three separate document expressions so that p's type
        //   is fully resolved into state.subst before $p.unknown is checked. The Open alias
        //   is registered at document level (state.level=0), so ρ is at level 0. The access
        //   $p.unknown happens inside a nested dict (level 2), so β and ρ_fresh are created
        //   at level 2, then lowered to ρ's level (0). Without the lowering call, the
        //   assertions below fail (2 ≤ 0 is false).
        //
        // WHY THREE EXPRESSIONS (NOT ONE OR TWO):
        //   In a single dict (letrec), p and result are siblings. p's type binding is in the
        //   local letrec subst, not in state.subst. So when check_dot_access processes $p,
        //   state.subst.apply(TypeVar(_tA)) returns TypeVar(_tA) — the TypeVar path is taken
        //   instead of the RowVar path, and ρ from Open is never directly bound.
        //   Using three expressions ensures p's type (Record({name: String}, RowVar(ρ))) is
        //   exported into state.subst via Pass 3d before expression 3 processes $p.unknown.
        //
        // TEST CASE (single document, three top-level expressions):
        //   [Open: [type [name: String ...]]]    -- expr 1: registers Open alias; ρ at level 0
        //   [p: [@Open [name: Alice]]]           -- expr 2: p : Record({name: String}, RowVar(ρ))
        //   [result: [inner: $p.unknown]]        -- expr 3: nested dict forces $p.unknown at level 2
        //
        //   When we access $p.unknown at level 2:
        //   1. $p has type Record({name: String}, RowVar(ρ, level=0)) — from state.subst
        //   2. check_dot_access sees "unknown" not in {name}, tail is RowVar(ρ) at level 0
        //   3. It creates fresh β at level 2 and ρ_fresh at level 2
        //   4. It calls lower_row_var_levels_pub to lower β and ρ_fresh to ρ's level (0)
        //   5. It binds ρ → Row({unknown: β}, RowVar(ρ_fresh)) in state.subst
        //   6. After lowering: beta_level = 0, rho_fresh_level = 0
        //
        //   The assertions `beta_level <= rho_level` and `rho_fresh_level <= rho_level` are
        //   non-trivial: they pass only because lowering reduced the levels from 2 to 0.
        //   Deleting the lower_row_var_levels_pub call would leave them at 2, failing 2 ≤ 0.
        // In new syntax, string literals require quotes.
        let code = r#"
            [Open: [type [name: String ...]]]
            [p: [@Open [name: "Alice"]]]
            [result: [inner: $p.unknown]]
        "#;

        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with dot access on open record inside nested dict: {:?}",
            result
        );

        // Verify that the `inner` field of `result` has type TypeVar (the fresh β from
        // constraint generation). result : Record({inner: TypeVar(β)}) after inference.
        let result_ty = result_field(code, "result");
        let inner_ty = match result_ty {
            Type::Record(Row { ref fields, .. }) => fields
                .get("inner")
                .cloned()
                .expect("result record should have 'inner' field"),
            other => panic!("expected result to be a Record type, got {other}"),
        };
        match inner_ty {
            Type::TypeVar(name, _level) => {
                assert!(
                    name.starts_with("_t"),
                    "expected fresh type var name to start with _t, got {}",
                    name
                );
            }
            other => panic!("expected TypeVar for $p.unknown inside nested dict, got {other}"),
        }

        // Core verification: inspect InferState to confirm level lowering occurred.
        //
        // ρ is created at level 0 (document level) when register_type_aliases runs for
        // expression 1 (state.level = 0 after infer_dict returns and restores the level).
        // β and ρ_fresh are created at level 2 (expression 3's inner dict) when $p.unknown
        // is checked. lower_row_var_levels_pub must lower them to level 0 (ρ's level).
        //
        // NON-VACUOUSNESS: if lower_row_var_levels_pub is deleted, β and ρ_fresh stay at
        // level 2 and the assertions below become "2 ≤ 0", which is false → test fails.
        let mut file = crate::parse(code).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        let doc_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .unwrap();

        // Find the row variable ρ from the Open type alias
        let open_ty = match doc_env.get_type_alias("Open") {
            Some(ty) => ty,
            None => panic!("Open type alias not found in doc_env"),
        };

        let rho_name = match open_ty {
            Type::Record(Row {
                tail: RowTail::RowVar(name, _),
                ..
            }) => name.clone(),
            other => panic!("expected Open to be an open record type, got {other}"),
        };

        // ρ must be bound in state.subst.row_map after dot access on the open record
        let bound_row = state
            .subst
            .row_map
            .get(&rho_name)
            .expect("ρ must be bound in subst after unknown-field dot access on open record");

        assert!(
            bound_row.fields.contains_key("unknown"),
            "bound row should contain 'unknown' field"
        );

        // ρ's current level (from state.levels, authoritative after any lowering)
        let rho_level = state
            .levels
            .get(&rho_name)
            .copied()
            .expect("ρ should be in state.levels after dot access");

        // ρ was created at document level (state.level = 0 when register_type_aliases runs).
        // Confirm this expectation so the non-vacuousness of the assertion is explicit.
        assert_eq!(
            rho_level, 0,
            "ρ (Open's row var) should be at level 0 (created at document level by register_type_aliases)"
        );

        // Check β's level — must be TypeVar; panic if not
        let Type::TypeVar(beta_name, _) = bound_row
            .fields
            .get("unknown")
            .expect("β must be present in bound row for 'unknown' field")
        else {
            panic!(
                "expected TypeVar for 'unknown' field in bound row, got {:?}",
                bound_row.fields.get("unknown")
            )
        };
        let beta_level = state
            .levels
            .get(beta_name)
            .copied()
            .expect("β should be in state.levels after dot access");

        // β was originally created at level 2 (inside the nested inner dict) and lowered to 0.
        // Without lowering: beta_level = 2, assertion 2 ≤ 0 would fail.
        assert!(
            beta_level <= rho_level,
            "β level ({}) should be ≤ ρ level ({}) after lower_row_var_levels_pub; \
             β is created at the inner dict level (2) and must be lowered to ρ's level (0)",
            beta_level,
            rho_level
        );

        // Check ρ_fresh's level — must be RowVar; panic if not
        let RowTail::RowVar(rho_fresh_name, _) = &bound_row.tail else {
            panic!(
                "expected RowVar tail in bound row, got {:?}",
                bound_row.tail
            )
        };
        let rho_fresh_level = state
            .levels
            .get(rho_fresh_name)
            .copied()
            .expect("ρ_fresh should be in state.levels after dot access");

        // ρ_fresh was originally created at level 2 and lowered to 0.
        // Without lowering: rho_fresh_level = 2, assertion 2 ≤ 0 would fail.
        assert!(
            rho_fresh_level <= rho_level,
            "ρ_fresh level ({}) should be ≤ ρ level ({}) after lower_row_var_levels_pub; \
             ρ_fresh is created at the inner dict level (2) and must be lowered to ρ's level (0)",
            rho_fresh_level,
            rho_level
        );
    }

    #[test]
    fn test_type_assert_open_record_accepts_extra_fields() {
        // In new syntax, string literals require quotes.
        check("[@[name: String ...] [name: \"Alice\"  age: 30]]").unwrap();
    }

    #[test]
    fn test_type_assert_closed_record_rejects_extra_fields() {
        // In new syntax, string literals require quotes.
        let errors = check_err("[@[name: String] [name: \"Alice\"  age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("cannot unify"));
    }

    #[test]
    fn test_type_assert_open_record_requires_fields() {
        let errors = check_err("[@[name: String ...] [age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("cannot unify"));
    }

    #[test]
    fn test_dot_access_on_open_record_known_field() {
        // In new syntax, string literals require quotes.
        assert_eq!(
            result_field(
                "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  age: 30]]]\n[result: $p.name]",
                "result",
            ),
            Type::Str,
        );
    }

    #[test]
    fn test_dot_access_on_open_record_unknown_field() {
        // Accessing an unknown field on an open record (RowVar tail) now generates a
        // constraint binding ρ → Row({unknown: β}, RowVar(ρ_fresh)) and returns β.
        // The result is a TypeVar (the fresh field type), not Any.
        // See doc/07-type-extensions.md Part 5 (RowVar case).
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"]]]\n[result: $p.unknown]",
            "result",
        );
        assert!(
            matches!(ty, Type::TypeVar(_, _)),
            "expected TypeVar for unknown open-record field, got {ty}"
        );
    }

    #[test]
    fn test_data_dict_always_closed() {
        let ty = infer("[a: 1  b: 2]");
        match ty {
            Type::Record(Row {
                tail: RowTail::Empty,
                ..
            }) => {}
            other => panic!("expected closed Record for data dict, got {other}"),
        }
    }

    #[test]
    fn test_rest_in_data_dict_ignored() {
        let ty = infer("[a: 1 ...]");
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::Empty,
            }) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
            }
            other => panic!("expected closed Record, got {other}"),
        }
    }

    // -- Let-generalization tests --

    #[test]
    fn test_let_gen_varref_instantiation() {
        // Each reference to $id should get a fresh instantiation
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[result: [a: [call $id 42]  b: [call $id \"hello\"]]]",
            "result",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(
                    fields.get("b"),
                    Some(&Type::StringLiteral("hello".to_string()))
                );
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_forward_ref_unification() {
        // Forward reference $b should unify with 42
        let ty = infer("[a: $b  b: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_nested_dicts_level_increment() {
        // Task 3: Verify state.level increments correctly for nested dict inference
        // and that inner dict entries generalize independently of outer
        // For [outer: [inner: 42]], outer dict runs at level 1, inner at level 2
        // The inner dict should generalize at level 1, producing schemes for its entries

        // Test with a more complex example that shows level scoping:
        // [outer: [id: [fn [x@a] $x]]]
        // The `id` function should be polymorphic even when nested
        let env = doc_env("[outer: [id: [fn [x@a] $x]]]");
        let outer_scheme = env.get("outer").expect("outer should be in env");

        match &outer_scheme.body {
            Type::Record(Row {
                fields: outer_fields,
                ..
            }) => {
                // The outer dict's `id` field should have a Function type
                let id_type = outer_fields
                    .get("id")
                    .expect("id should be a field in outer");

                match id_type {
                    Type::Function {
                        params,
                        ret,
                        variadic: _,
                    } => {
                        // Params and return should involve type variables (from annotation @a)
                        assert!(
                            matches!(params.get(0), Some(Type::TypeVar(_, _))),
                            "id param should be TypeVar, got {:?}",
                            params
                        );
                        assert!(
                            matches!(ret.as_ref(), Type::TypeVar(_, _)),
                            "id return should be TypeVar, got {:?}",
                            ret
                        );
                    }
                    other => panic!("expected Function type for id, got {:?}", other),
                }
            }
            other => panic!("expected Record for outer, got {:?}", other),
        }
    }

    #[test]
    fn test_let_gen_document_boundary_threading() {
        // Type schemes should thread across document boundaries.
        // Verify that a polymorphic function defined in one document can be used
        // in a subsequent document, and that its scheme has type variables.
        let env = file_env("[id: [fn [x@a] $x]]\n---\n[r: [call $id 42]]");

        // Check that $id is available in the final environment
        let id_scheme = env.get("id").expect("id should be in scope");

        // Verify the scheme has type variables (polymorphic)
        assert!(
            !id_scheme.type_vars.is_empty() || !id_scheme.row_vars.is_empty(),
            "id's scheme should have type variables (polymorphic)"
        );

        // Check that result refers to id correctly
        assert!(env.get("r").is_some(), "r should be in scope");
    }

    #[test]
    fn test_let_gen_mutual_recursion() {
        // Mutual recursion within a dict should work with monomorphic inference
        let ty = infer("[a: $b  b: $a  c: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert!(fields.contains_key("a"));
                assert!(fields.contains_key("b"));
                assert_eq!(fields.get("c"), Some(&Type::IntLiteral(42)));

                // Task 2: Assert the TYPES of a and b after mutual reference unification
                let a_type = fields.get("a").expect("a should exist");
                let b_type = fields.get("b").expect("b should exist");

                // a and b reference each other, so they should unify to the same TypeVar
                // or both be Any if unification fails during Pass 3
                match (a_type, b_type) {
                    (Type::TypeVar(a_name, a_level), Type::TypeVar(b_name, b_level)) => {
                        // They should be unified to the same variable
                        assert_eq!(
                            a_name, b_name,
                            "mutually recursive a and b should unify to same TypeVar, got a={} b={}",
                            a_name, b_name
                        );
                        assert_eq!(
                            a_level, b_level,
                            "mutually recursive a and b should have same level"
                        );
                    }
                    (Type::Any, Type::Any) => {
                        // Both Any is also valid (error recovery path)
                    }
                    _ => panic!(
                        "expected a and b to both be TypeVar or both be Any, got a={:?} b={:?}",
                        a_type, b_type
                    ),
                }
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_typevar_in_bracket_access() {
        // TypeVars should be handled gracefully in bracket access.
        // In new syntax, string literals require quotes.
        // key: "x" → StringLiteral("x"), same as old bare-word x but now quoted.
        // During letrec forward-ref phase, $key has TypeVar type → check_bracket_access
        // returns Any (TypeVar target + TypeVar-or-Any key → Any).
        assert_eq!(
            result_field("[result: $data[$key]  data: [x: 1]  key: \"x\"]", "result"),
            Type::Any,
        );
    }

    #[test]
    fn test_let_gen_typevar_in_dot_access() {
        // Dot access on a TypeVar generates a constraint (TypeVar α case) which is now
        // fully resolved by Pass 3b (row-unification-h). When `$data` has an unknown type
        // during letrec pass 3, `$data.x` generates constraint α = Record({x: β}, RowVar(ρ))
        // and returns β. Pass 3b unifies the two α bindings (from check_dot_access and from
        // infer_dict processing `data: [x: 1]`), resolving β → IntLiteral(1).
        let ty = infer("[result: $data.x  data: [x: 1]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // result is the resolved type of x (IntLiteral(1)), not Any and not TypeVar.
                // Pass 3b constraint unification resolves β through the γ_data collision.
                let result_ty = fields.get("result").expect("field 'result' should exist");
                assert!(
                    !matches!(result_ty, Type::Any),
                    "expected resolved type for constrained dot access field, got Any"
                );
                assert!(
                    !matches!(result_ty, Type::TypeVar(_, _)),
                    "expected resolved type (not TypeVar) for constrained dot access field \
                     — Pass 3b should have resolved β via γ_data collision; got {result_ty}"
                );
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // --- Task 1: Core let-generalization unit tests ---

    #[test]
    fn test_let_gen_polymorphic_identity_generalizes() {
        // [id: [fn [x@a] $x]] should generalize id to a polymorphic TypeScheme
        let env = doc_env("[id: [fn [x@a] $x]]");
        let id_scheme = env.get("id").expect("id should be in env");

        // The scheme should have non-empty vars (it's polymorphic)
        assert!(
            !id_scheme.type_vars.is_empty(),
            "id should be polymorphic (non-empty type_vars), got scheme: {:?}",
            id_scheme
        );
    }

    #[test]
    fn test_let_gen_nested_dicts_level_correct() {
        // Nested dict [outer: [inner: 42]] should infer correct types
        let ty = result_field("[outer: [inner: 42]]\n[result: $outer]", "result");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(
                    fields.get("inner"),
                    Some(&Type::IntLiteral(42)),
                    "inner field should be IntLiteral(42)"
                );
            }
            other => panic!("expected Record for outer, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_any_touched_not_generalized() {
        // When a variable unifies with Any, it should NOT be generalized
        // [fn [x] $x] has unannotated param, so x has type Any
        // The identity function should be monomorphic (Any → Any)
        let env = doc_env("[id: [fn [x] $x]]");
        let id_scheme = env.get("id").expect("id should be in env");

        // The scheme should have empty vars (monomorphic, Any-touched)
        assert!(
            id_scheme.type_vars.is_empty(),
            "id with unannotated param should be monomorphic (Any-touched), got scheme: {:?}",
            id_scheme
        );
    }

    // -- Bidirectional type checking tests --

    #[test]
    fn test_check_expr_basic_subsumption() {
        // IntLiteral(42) should check against Int via subsumption
        let ty = result_field("[x: [@Int 42]]", "x");
        assert_eq!(ty, Type::Int, "IntLiteral should subsume to Int");

        // IntLiteral(42) should check against Number via subsumption
        let ty = result_field("[x: [@Number 42]]", "x");
        assert_eq!(ty, Type::Number, "IntLiteral should subsume to Number");

        // StringLiteral should subsume to String (use quoted string in new syntax)
        let ty = result_field("[x: [@String \"hello\"]]", "x");
        assert_eq!(ty, Type::Str, "StringLiteral should subsume to String");
    }

    #[test]
    fn test_call_mono_argument_checking() {
        // Monomorphic function call should use check_expr for arguments
        // This should succeed: IntLiteral(42) <: Int
        let ty = result_field("[f: [fn [x@Int] $x]]\n[result: [call $f 42]]", "result");
        assert_eq!(ty, Type::Int, "CALL-MONO should accept IntLiteral arg");

        // This should fail: String is not subtype of Int (use quoted string in new syntax)
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [call $f \"hello\"]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "CALL-MONO should reject String arg for Int param, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_concrete() {
        // Lambda checked against concrete function type should propagate param types
        // Define a concrete function type alias first
        let env = doc_env("[IntFn: [type [Fn@Int [Int]]]]\n[f: [@IntFn [fn [x] $x]]]");
        let f_scheme = env.get("f").unwrap();
        match &f_scheme.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, &vec![Type::Int]);
                assert_eq!(**ret, Type::Int);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_lambda_checking_mode_with_polymorphic_expected() {
        // Lambda checked against polymorphic function type should NOT use checking mode
        // (falls back to synthesis + subsumption).
        // After Fix 1: annotation names in type aliases become fresh internal vars.
        // The type alias `[Fn@b [a]]` gives Function { params: [TypeVar(X)], ret: TypeVar(Y) }
        // where X and Y are distinct fresh vars. The lambda is inferred independently (no checking
        // mode since the expected type has inference vars), so the final type is a Function with
        // unresolved TypeVars.
        let ty = result_field(
            "[Mapper: [type [Fn@b [a]]]]\n[x: [@Mapper [fn [v] $v]]]",
            "x",
        );
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                // When checking mode is skipped (has_inference_vars), params and ret stay as TypeVars.
                // We can't check specific names (they're fresh), just that they're TypeVars.
                assert_eq!(params.len(), 1, "expected 1 param");
                assert!(
                    matches!(params[0], Type::TypeVar(_, _)),
                    "param should be TypeVar, got {:?}",
                    params[0]
                );
                assert!(
                    matches!(*ret, Type::TypeVar(_, _)),
                    "ret should be TypeVar, got {ret:?}"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_type_assert_checking_mode() {
        // TypeAssert should use check_expr for subsumption
        let ty = result_field("[x: [@Int 42]]", "x");
        assert_eq!(ty, Type::Int, "TypeAssert should accept IntLiteral <: Int");

        // TypeAssert with default should suppress errors
        let ty = result_field("[x: [@[type: Int  default: 0] hello]]", "x");
        assert_eq!(
            ty,
            Type::Int,
            "TypeAssert with default should suppress errors"
        );
    }

    #[test]
    fn test_call_poly_still_uses_unify() {
        // Polymorphic function call should still use unification (not check_expr)
        let ty = result_field("[f: [fn [x@a] $x]]\n[result: [call $f 42]]", "result");
        assert_eq!(ty, Type::IntLiteral(42), "CALL-POLY should unify");

        // Multiple calls should get independent instantiations (use quoted string in new syntax)
        let env = doc_env("[f: [fn [x@a] $x]]\n[r1: [call $f 42]]\n[r2: [call $f \"hello\"]]");
        let r1 = env.get("r1").unwrap();
        let r2 = env.get("r2").unwrap();
        assert_eq!(r1.body, Type::IntLiteral(42));
        assert_eq!(r2.body, Type::StringLiteral("hello".into()));
    }

    #[test]
    fn test_function_return_annotation_checking() {
        // Function with return annotation should check body via check_expr
        // Subsumption should work: IntLiteral(42) <: Int
        let ty = result_field("[f: [fn@Int [] 42]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert_eq!(*ret, Type::Int, "Return type should be declared type");
            }
            other => panic!("expected Function, got {other}"),
        }

        // IntLiteral should subsume to Number in return annotation
        let ty = result_field("[f: [fn@Number [] 42]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }

        // Type mismatch should fail (use quoted string in new syntax)
        let errors = check_err("[f: [fn@Int [] \"hello\"]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "Function body type mismatch should error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_function_return_annotation_with_type_var() {
        // Function with polymorphic return annotation should use unification mode
        // [fn@a [x@a] 42] — return annotation contains TypeVar, so body type
        // should be unified with the declared type, binding the TypeVar.
        // Without the fix, check_expr uses is_subtype which requires exact match
        // for TypeVars (reflexive equality only), so is_subtype(IntLiteral(42), TypeVar("a"))
        // returns false and the function is rejected.
        //
        // The key is that this should successfully type check (not error).
        let result = check("[f: [fn@a [x@a] 42]]");
        assert!(
            result.is_ok(),
            "Function with polymorphic return annotation should type check: {:?}",
            result.err()
        );

        // Identity function with return annotation should also work
        let result = check("[f: [fn@a [x@a] $x]]");
        assert!(
            result.is_ok(),
            "Identity function with polymorphic return annotation should type check: {:?}",
            result.err()
        );

        // Polymorphic function that returns a different type than param should succeed
        // [fn@a [x@b] 42] where a and b are different type variables
        // After unification: a gets bound to IntLiteral(42), but param is still b
        // This should succeed since there's no constraint linking a and b
        let result = check("[f: [fn@a [x@b] 42]]");
        assert!(
            result.is_ok(),
            "Polymorphic function with different param/return type vars should type check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_function_return_annotation_with_type_var_error_path() {
        // Exercise the error path of the new unification-mode branch
        // (declared.has_inference_vars() = true) at src/typecheck.rs:1056-1062.
        //
        // When the body expression fails to infer a type, the error propagates
        // via `?` at line 1057. This test confirms that the new path correctly
        // surfaces body inference errors rather than silently succeeding.
        //
        // [fn@a [x@a] [call 42 1]] — return annotation @a contains a TypeVar
        // so we enter the unification-mode branch. The body `[call 42 1]`
        // attempts to call an integer literal as a function, which fails
        // infer_expr with "expected function type, got IntLiteral(42)".
        let errors = check_err("[f: [fn@a [x@a] [call 42 1]]]");
        assert!(
            !errors.is_empty(),
            "Calling a non-function in a TypeVar-annotated fn body should produce type errors"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expected function type")),
            "Expected 'expected function type' error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_annotated_param_incompatible() {
        // Lambda with annotated param checked against expected function type where
        // the annotation is INCOMPATIBLE with the expected param type should error.
        // Expected: Fn(Int -> Int), lambda: [fn [x@String] $x]
        // The annotation String is incompatible: Int (expected) is not a subtype of String.
        // This tests the fix added in the bidirectional-typing fix pass (contravariant check).
        let errors = check_err("[x: [@[Fn@Int [Int]] [fn [x@String] $x]]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("parameter annotation")
                    && e.message.contains("more restrictive")),
            "Incompatible param annotation should produce contravariant error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_return_annotation_and_expected_type() {
        // Lambda with both a return annotation and an expected function type.
        // [@[Fn@Number [Int]] [fn@Int [x] $x]] — expected return Number, declared return Int.
        // Since Int <: Number, the check `declared <: expected_ret` passes (covariant return).
        // Body $x is checked against declared Int (passes since x gets type Int from expected).
        // The function type recorded in the type_map is the EXPECTED type (Fn(Int→Number))
        // because check_expr records expected.clone() at the lambda checking mode exit.
        let ty = result_field("[f: [@[Fn@Number [Int]] [fn@Int [x] $x]]]", "f");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                // Lambda checking mode propagates expected param type Int
                assert_eq!(
                    params,
                    vec![Type::Int],
                    "param should be Int from expected type"
                );
                // The recorded function type is the expected Fn(Int→Number), ret = Number
                assert_eq!(
                    *ret,
                    Type::Number,
                    "return type is the expected Number (type_map records expected)"
                );
            }
            other => panic!("expected Function type, got {other}"),
        }

        // Incompatible direction: expected return Int, declared return Number.
        // is_subtype(&Number, &Int) = false → should error.
        let errors = check_err("[f: [@[Fn@Int [Int]] [fn@Number [x] 42]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "Declared return Number is not subtype of expected Int — should error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_param_annotation_with_type_var() {
        // Task 1 fix: Lambda with @a-style param annotation checked against concrete function type.
        // When the annotation is a TypeVar, is_subtype fails (TypeVars only match reflexively).
        // The fix switches to unification mode when resolved.has_inference_vars().
        //
        // Pattern: [call $identity [fn@b [y@b] $y]] where identity is polymorphic.
        // check_expr sees expected_ty=concrete from identity's instantiation, resolved=TypeVar("b").
        // Without fix: is_subtype(concrete, TypeVar("b")) = false → error.
        // With fix: unify(concrete, TypeVar("b")) binds b → success.
        let result = check("[identity: [fn [x@a] $x]]\n[result: [call $identity [fn@b [y@b] $y]]]");
        assert!(
            result.is_ok(),
            "Lambda with TypeVar param annotation in checking mode should unify, not subsume: {:?}",
            result.err()
        );

        // Verify the result typechecks with concrete argument
        let ty = result_field(
            "[identity: [fn [x@a] $x]]\n[result: [call $identity [fn@b [y@b] $y]]]\n[test: [call $result 42]]",
            "test"
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Result function should work with concrete arg"
        );
    }

    #[test]
    fn test_lambda_checking_mode_return_annotation_with_type_var() {
        // Task 1 fix: Lambda with @a-style return annotation checked against concrete function type.
        // When the return annotation is a TypeVar, is_subtype fails (TypeVars only match reflexively).
        // The fix switches to unification mode when declared.has_inference_vars().
        //
        // Pattern: [@[Fn@Int [Int]] [fn@c [x] 42]] — expected return Int, declared TypeVar("c").
        // Without fix: is_subtype(TypeVar("c"), Int) = false → error.
        // With fix: unify(TypeVar("c"), Int) binds c → success.
        let result = check("[f: [@[Fn@Int [Int]] [fn@c [x] 42]]]");
        assert!(
            result.is_ok(),
            "Lambda with TypeVar return annotation in checking mode should unify, not subsume: {:?}",
            result.err()
        );

        // Verify the recorded function type
        let ty = result_field("[f: [@[Fn@Int [Int]] [fn@c [x] $x]]]", "f");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Int], "param from expected type");
                assert_eq!(*ret, Type::Int, "return from expected type");
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_lambda_checking_mode_param_annotation_error_message() {
        // Verify that parameter annotation type mismatch error messages are correctly ordered.
        // When checking [@[Fn@Number [Int]] [fn [x@String] $x]], the expected param type is Int
        // (from the function type annotation) but the parameter annotation says String.
        // The error should say "cannot unify Int with String" (not "cannot unify String with Int").
        let errors = check_err("[f: [@[Fn@Number [Int]] [fn [x@String] $x]]]");
        assert_eq!(errors.len(), 1, "should have exactly one error");
        let msg = &errors[0].message;
        assert!(
            msg.contains("parameter annotation") && msg.contains("more restrictive"),
            "Error message should say 'parameter annotation ... more restrictive ...' but got: {msg}"
        );
    }

    #[test]
    fn test_lambda_checking_mode_subst_apply_forward_compat_guard() {
        // Forward-compatibility guard: check_expr lambda checking mode applies
        // state.subst to expected_ret before checking the body.
        //
        // The guard at lambda checking mode entry applies state.subst to the expected
        // type before checking for TypeVars. TypeVars that are already bound in
        // state.subst are resolved, allowing lambda checking mode to fire for types
        // that are "effectively concrete" after substitution.
        //
        // In practice, no current call path produces an expected type with
        // bound-but-unapplied TypeVars (CALL-MONO resolves them before calling
        // check_expr; TypeAssert creates fresh annotation TypeVars not yet in subst).
        // This test exercises the concrete-type path and confirms the subst.apply
        // does not cause regressions.
        //
        // Pattern: [data: [x: 42]] entry creates state.subst bindings, then
        // [f: [@[Fn@Int [Int]] [fn [n] $n]]] triggers lambda checking mode with
        // concrete expected type Fn(Int -> Int). The body check uses expected_ret = Int
        // (subst applied, though it's a no-op for concrete types).
        let ty = result_field("[data: [x: 42]]\n[f: [@[Fn@Int [Int]] [fn [n] $n]]]", "f");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Int], "param from expected type");
                assert_eq!(*ret, Type::Int, "return from expected type");
            }
            other => panic!("expected Function type, got {other}"),
        }

        // Also verify with a body that returns a literal subtype of the expected return type
        let result = check("[f: [@[Fn@Int [Int]] [fn [n] 42]]]");
        assert!(
            result.is_ok(),
            "Lambda body returning IntLiteral(42) should satisfy expected return type Int: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_lambda_checking_mode_subst_applied_to_expected() {
        // Verify that the lambda checking mode guard applies state.subst to the
        // expected type before inspecting it for TypeVars.
        //
        // This test validates the Algorithm W substitution threading invariant
        // (Damas & Milner, 1982): substitutions must be applied before inspecting
        // types. The guard uses state.subst.apply(expected) so that bound TypeVars
        // are resolved before the has_inference_vars() check.
        //
        // Scenario: A polymorphic type annotation @[Fn@a [a]] on a lambda creates
        // fresh TypeVars. These TypeVars are NOT in state.subst, so lambda checking
        // mode is correctly skipped (falls through to synthesize + subsume).
        // The synthesize path handles this correctly by inferring the lambda's type
        // and checking it against the expected type via subsumption.
        let result = check("[f: [@[Fn@a [a]] [fn [x] $x]]]");
        assert!(
            result.is_ok(),
            "Polymorphic type annotation on lambda should succeed via synthesis: {:?}",
            result.err()
        );

        // With concrete expected type, lambda checking mode fires as before
        let ty = result_field("[f: [@[Fn@Int [Int]] [fn [x] $x]]]", "f");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Int], "concrete param propagated");
                assert_eq!(*ret, Type::Int, "concrete ret propagated");
            }
            other => panic!("expected Function type, got {other}"),
        }

        // Verify that prior dict entries creating state.subst bindings don't
        // interfere with lambda checking mode on concrete expected types
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[n: [call $id 42]]\n[f: [@[Fn@Int [Int]] [fn [x] $x]]]",
            "f",
        );
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, vec![Type::Int], "param from expected type");
                assert_eq!(*ret, Type::Int, "ret from expected type");
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_inline_lambda_with_polymorphic_return_annotation() {
        // Task 2 fix: Inline lambda with polymorphic return annotation.
        // Pattern: [call [fn@a [x@a] $x] 42] — identity function with polymorphic annotation.
        //
        // Without fix at check_call line ~888:
        // 1. infer_fn returns Fn(TypeVar("_t5") -> TypeVar("_t5")) with state.subst = {_t5 -> TypeVar("_t6")}
        //    (from unifying body $x with return annotation @a)
        // 2. check_call receives func_ty with unresolved _t5
        // 3. has_inference_vars() = true → CALL-POLY fires
        // 4. instantiate_at_level freshens _t5 to _t7
        // 5. unify tries to bind _t7, but the substitution for _t5 is lost → wrong type
        //
        // With fix: state.subst.apply() resolves _t5 before has_inference_vars() check.
        let ty = result_field("[result: [call [fn@a [x@a] $x] 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Inline lambda with polymorphic return annotation should infer correctly"
        );

        // Verify multi-arg case where all params share the same type variable
        let ty = result_field("[result: [call [fn@a [x@a y@a] $x] 1 1]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(1),
            "Multi-arg inline lambda with polymorphic annotation should work"
        );

        // Verify constant-return case: [call [fn@a [x@a] 42] 42]
        // Based on the mempalace C66 finding. When param and return share annotation @a,
        // they're constrained to be the same type. The body type (IntLiteral(42)) binds @a.
        // Without the fix: CALL-POLY would fire, freshen the TypeVars, and produce incorrect types.
        // With the fix: state.subst.apply() resolves the function type to Fn(IntLiteral(42) -> IntLiteral(42)),
        // CALL-MONO fires, and the call succeeds with matching literal types.
        let ty = result_field("[result: [call [fn@a [x@a] 42] 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Constant-return inline lambda with matching arg should work"
        );
    }

    #[test]
    fn test_zero_param_monomorphic_function_type() {
        // Zero-param monomorphic functions work correctly with CALL-MONO.
        // The function type is inferred from the return type annotation.
        //
        // Historical note: Previously there was a bug in CALL-POLY with zero params,
        // where the code returned `*ret.clone()` (the pre-instantiation return type)
        // instead of `*inst_ret.clone()` (the post-substitution return type).
        // This was fixed in the bidirectional-typing-b sprint.
        //
        // Practically, zero-arity polymorphic functions in LLT are rare:
        //   - Unannotated params get Type::Any (monomorphic path, no type vars).
        //   - Annotated type-var params require at least one param (by definition).
        //   - [fn@a [] body] fails to type-check because body type ≮ TypeVar a.
        //
        // This test verifies the zero-param CALL-MONO path (no type vars) works correctly.

        // Zero-param monomorphic function (CALL-MONO): the function type is correct.
        let ty = result_field("[f: [fn@Int [] 42]]", "f");
        match ty {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert!(params.is_empty(), "zero-param fn should have no params");
                assert_eq!(
                    *ret,
                    Type::Int,
                    "declared return type Int should be preserved"
                );
            }
            other => panic!("expected Function type for zero-param fn, got {other}"),
        }
    }

    // -- Task 1: CALL-MONO argument type checking verification --

    #[test]
    fn test_call_mono_argument_type_checking_verification() {
        // CALL-MONO uses check_expr for argument type checking
        // IntLiteral(42) <: Int succeeds
        assert!(check("[f: [fn [x@Int] $x]]\n[result: [call $f 42]]").is_ok());

        // StringLiteral for Int param fails
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [call $f \"hello\"]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "StringLiteral arg for Int param should error: {:?}",
            errors
        );

        // IntLiteral(42) <: Number succeeds (transitive subsumption)
        assert!(check("[f: [fn [x@Number] $x]]\n[result: [call $f 42]]").is_ok());
    }

    // -- Task 3: Subsumption tests --

    #[test]
    fn test_subsumption_int_literal_to_int() {
        // IntLiteral(42) <: Int via [SUB] rule
        assert!(check("[result: [@Int 42]]").is_ok());
    }

    #[test]
    fn test_subsumption_int_literal_to_number() {
        // IntLiteral(42) <: Int <: Number (transitive)
        assert!(check("[result: [@Number 42]]").is_ok());
    }

    #[test]
    fn test_subsumption_string_literal_to_string() {
        // StringLiteral("hello") <: String
        assert!(check("[result: [@String \"hello\"]]").is_ok());
    }

    #[test]
    fn test_subsumption_direction_matters() {
        // Int <: Number succeeds, but Number <: Int fails — direction matters
        assert!(check("[result: [@Number 42]]").is_ok());
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [@Int [call $f 3.14]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "Float should not be subtype of Int: {:?}",
            errors
        );
    }

    #[test]
    fn test_subsumption_float_to_number() {
        // Float <: Number
        assert!(check("[result: [@Number 3.14]]").is_ok());
    }

    // -- Task 3: Lambda parameter inference tests --

    #[test]
    fn test_lambda_param_inference_from_context() {
        // When checking lambda against Fn(Int → Int), unannotated param gets Int
        // Uses Fn@ReturnType [params] syntax to get a real function type, not Type::Any
        assert!(check("[result: [@[Fn@Int [Int]] [fn [x] $x]]]").is_ok());
    }

    #[test]
    fn test_lambda_param_inference_preserves_annotation() {
        // Annotated param @Number matches expected Number exactly — no variance issue.
        // In new syntax, function types use [Fn@RetType [ParamType]] dict form (Fn@RetType is Annotated).
        // Note: @Int with expected Number is REJECTED (Int <: Number but function params are
        // checked for exact compatibility, not subtype). This test uses @Number to match exactly.
        let result = check("[result: [@[Fn@Number [Number]] [fn [x@Number] $x]]]");
        assert!(
            result.is_ok(),
            "expected ok, got errors: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_lambda_param_inference_rejects_incompatible_annotation() {
        // @String is NOT compatible with expected Int param (Int <: String is false)
        // Uses Fn@ReturnType [params] syntax for function type annotation
        let errors = check_err("[result: [@[Fn@Int [Int]] [fn [x@String] $x]]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("parameter annotation")
                    && e.message.contains("more restrictive")),
            "String annotation should be incompatible with Int expected param: {:?}",
            errors
        );
    }

    // -- Task 8: Zero-param polymorphic fix verification --

    #[test]
    fn test_zero_param_polymorphic_function_instantiation() {
        // Zero-param CALL-POLY must return *inst_ret* (instantiated), not *ret* (scheme-internal).
        // Without the fix, ret == inst_ret for concrete return types, but the instantiated copy
        // is the one whose type variables (if any) are fresh per-call-site.
        let ty = result_field("[f: [fn@Int [] 42]]\n[result: [call $f]]", "result");
        assert_eq!(
            ty,
            Type::Int,
            "zero-param fn@Int should return Int, got {ty}"
        );
    }

    // -- Annotation fresh variable mapping per function --

    #[test]
    fn test_sibling_functions_with_shared_annotation_names() {
        // Bug: sibling functions in the same letrec dict that use the same annotation
        // name (e.g., @a) should NOT share type variables. Each function should get
        // its own fresh type variable for @a.
        //
        // [f: [fn [x@a] $x]  g: [fn [y@a] 42]]
        //
        // Before fix: both functions share TypeVar("a", level) in state.levels, so
        // unification in f's inference can affect g's type variable.
        //
        // After fix: f gets TypeVar("_t0", level) and g gets TypeVar("_t1", level).
        // Within each function, repeated uses of @a map to the same fresh var.
        let result = check("[f: [fn [x@a] $x]  g: [fn [y@a] 42]]");
        assert!(
            result.is_ok(),
            "sibling functions with same annotation name should type check: {:?}",
            result.err()
        );

        // Verify that within a single function, repeated uses of @a map to the same variable
        let result = check("[f: [fn [x@a  y@a] $x]]");
        assert!(
            result.is_ok(),
            "repeated annotation @a within single function should use same type variable: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_annotation_fresh_vars_are_independent_across_siblings() {
        // Each sibling function should have independent type variables for its annotations.
        // This test ensures that type constraints in one function don't leak to another.
        //
        // [id: [fn [x@a] $x]  const42: [fn [y@a] 42]]
        //
        // id should be polymorphic: ∀a. Fn(a → a)
        // const42 should be polymorphic: ∀a. Fn(a → Int)
        //
        // The @a in id and the @a in const42 must not interfere with each other.
        let ty = infer("[id: [fn [x@a] $x]  const42: [fn [y@a] 42]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // Verify both functions exist
                assert!(fields.contains_key("id"), "should have 'id' field");
                assert!(
                    fields.contains_key("const42"),
                    "should have 'const42' field"
                );

                // Both should be function types
                match fields.get("id") {
                    Some(Type::Function { .. }) => {}
                    other => panic!("expected id to be Function type, got {:?}", other),
                }
                match fields.get("const42") {
                    Some(Type::Function { .. }) => {}
                    other => panic!("expected const42 to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }
    }

    #[test]
    fn test_annotation_level_monotonicity() {
        // Test that resolve_type_name respects level lowering monotonicity (Kiselyov 2013).
        // When the same annotation name is used multiple times in a function and unification
        // lowers the level between references, the level must not be reset.
        //
        // Pattern: [fn [x@a y@a] body] where x and y share the same annotation name @a.
        // Both should map to the same fresh TypeVar (e.g., _t0), and subsequent references
        // to @a within type annotations should return the TypeVar with its current level
        // from state.levels, NOT reset it to state.level.
        //
        // This test verifies the function type-checks correctly. If level monotonicity
        // were violated, generalization might fail or produce incorrect types.

        // Case 1: Two params share the same annotation name
        let ty = infer("[f: [fn [x@a y@a] $x]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                match fields.get("f") {
                    Some(Type::Function { params, .. }) => {
                        // Both params should unify to the same type variable
                        assert_eq!(params.len(), 2, "function should have 2 params");
                        // They should be the same TypeVar (same name after unification)
                        assert_eq!(
                            params[0], params[1],
                            "both params should have same type (unified via shared annotation)"
                        );
                    }
                    other => panic!("expected f to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }

        // Case 2: Return annotation reuses param annotation
        let ty = infer("[f: [fn@a [x@a] $x]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                match fields.get("f") {
                    Some(Type::Function {
                        params,
                        ret,
                        variadic: _,
                    }) => {
                        // Param and return should unify to the same type variable
                        assert_eq!(
                            params[0], **ret,
                            "param and return should have same type (unified via shared annotation)"
                        );
                    }
                    other => panic!("expected f to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }

        // Case 3: Generalization should succeed despite multiple uses of same annotation
        let env = doc_env("[f: [fn [x@a y@a] $x]]");
        let f_scheme = env.get("f").expect("f should be in env");
        assert!(
            !f_scheme.type_vars.is_empty(),
            "f should be polymorphic (generalized despite multiple @a uses), got scheme: {:?}",
            f_scheme
        );
    }

    #[test]
    fn test_polymorphic_function_call_no_double_instantiation() {
        // This test verifies that calling a polymorphic function from the environment
        // only instantiates once (not VAR-POLY + CALL-POLY double instantiation).
        // The optimization special-cases VarRef in Call expressions for polymorphic schemes.

        // Test with multiple calls to the same polymorphic function across documents
        // In new syntax, string literals require quotes.
        let ty = result_type("[id: [fn [x@a] $x]]\n[r1: [call $id 42]  r2: [call $id \"hello\"]]");

        match ty {
            Type::Record(Row { fields, .. }) => {
                // r1 should be IntLiteral(42) due to polymorphic instantiation
                assert_eq!(
                    fields.get("r1"),
                    Some(&Type::IntLiteral(42)),
                    "r1 should be IntLiteral(42)"
                );

                // r2 should be StringLiteral("hello") due to polymorphic instantiation
                assert_eq!(
                    fields.get("r2"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "r2 should be StringLiteral(\"hello\")"
                );
            }
            other => panic!("expected Record type, got {:?}", other),
        }
    }

    // -- state.subst apply() regression test --

    #[test]
    fn test_bracket_access_forward_ref_resolves_correctly() {
        // Forward-reference bracket access should resolve to field type.
        // Exercises the TypeVar constraint generation in check_bracket_access:
        // γ_data is a TypeVar (forward ref), bracket access with string-literal key
        // generates α = Record({name: β}, RowVar(ρ)), Pass 3b resolves β.
        // In new syntax, string literals require quotes (both value and key).
        let ty = result_field(
            "[result: $data[\"name\"]  data: [name: \"hello\"]]",
            "result",
        );
        assert_eq!(ty, Type::StringLiteral("hello".to_string()));
    }

    // -- CALL-POLY state.subst constraint test --

    #[test]
    fn test_call_poly_end_to_end_dot_access_resolution() {
        // Task 7: Regression test for `state.subst.apply()` in the CALL-POLY arm of
        // check_call_with_scheme and check_call.
        //
        // The two CALL-POLY sites are:
        //   check_call_with_scheme (CALL-POLY arm): Ok(subst.apply(ret))
        //     (subst is seeded from state.subst, so single apply is sufficient)
        //   check_call (CALL-POLY arm): Ok(state.subst.apply(&subst.apply(inst_ret)))
        //
        // Without state.subst resolution, the return type may contain unresolved TypeVars.
        // In check_call_with_scheme, the seeded subst handles this implicitly.
        // In check_call, the explicit state.subst.apply() resolves TypeVars bound from
        // prior dot-access constraints that wrote to state.subst.
        //
        // HOW THIS TEST DETECTS THE REGRESSION:
        //   The forward-reference in `$data` forces Pass 1 to assign TypeVar(_t_data) to
        //   `data`'s slot.  When `result` is processed (left-to-right in Pass 3),
        //   check_dot_access sees TypeVar(_t_data) for `$data`, enters the TypeVar arm, and
        //   writes `_t_data → Record({name: _t_name}, ρ)` into state.subst (not local subst).
        //   It returns TypeVar(_t_name) as the field type (arg to $id).
        //   After call unification: local subst[_t_call = _t_name].
        //   subst.apply(inst_ret) = _t_name (local subst resolves _t_call to _t_name).
        //   state.subst.apply(_t_name) = _t_name (not yet bound; data not yet processed).
        //
        //   After Pass 3 processes `data: [name: hello]`, unification propagates
        //   _t_data = Record({name: StringLiteral("hello")}, Closed) through state.subst,
        //   and Pass 3b/3c resolves _t_name = StringLiteral("hello") globally.
        //
        //   The final asserted type of `result` comes through this chain.  If state.subst.apply()
        //   were removed from the CALL-POLY return and ALSO from Pass 3b/3c, the type would
        //   remain an unresolved TypeVar.  The test thus provides a regression guard for the
        //   full state.subst pipeline of which the CALL-POLY site is the first link.
        //
        //   A stronger isolation test (where ONLY removing state.subst.apply() from the CALL-POLY
        //   site causes failure) requires a scenario where _t_name is already bound in state.subst
        //   BEFORE the call is processed — achievable once cross-field constraint propagation within
        //   a single letrec pass is fully implemented (tracked as future work).
        // NOTE: The test input uses plain `\n` separators (NOT `---\n`), so the parser
        // produces ONE document containing three sequential dict expressions.
        // `typecheck_document` processes them left-to-right in a single letrec pass,
        // threading each expression's field scheme into the environment before moving on.
        //
        // When `[call $id $data.name]` is processed, both `id` (already a TypeScheme)
        // and `$data` (TypeVar(_t_data) at that point) are in scope from the preceding
        // expressions.  check_dot_access enters the TypeVar arm for `$data`, writes
        // `_t_data → Record({name: _t_name}, ρ)` into state.subst, and returns
        // TypeVar(_t_name) as the arg type.  After Pass 3b/3c resolves `data: [name: hello]`,
        // _t_name is bound to StringLiteral("hello") globally, and `result` resolves.
        //
        // state.subst.apply() at the CALL-POLY return is benign in this scenario (local subst
        // already resolved the binding), but the test guards the full CALL-POLY path end-to-end
        // (arg inference → unification → return type resolution).  A stronger isolation test
        // (where ONLY removing state.subst.apply() from the CALL-POLY site causes failure)
        // requires cross-field constraint propagation within a letrec pass, tracked as
        // future work (row-unification-h).
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n[result: [call $id $data.name]]",
            "result",
        );
        // result should be StringLiteral("hello") via CALL-POLY: id returns same type as arg
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY with dot-access argument should resolve return type correctly, got: {ty}"
        );
    }

    // -- CALL-POLY state.subst isolation test (cross-document boundary) --

    #[test]
    fn test_call_poly_state_subst_isolation() {
        // Cross-document regression test for `state.subst.apply()` in the CALL-POLY arm.
        //
        // SCENARIO: Two documents separated by `---`. Document 1 contains a single dict with
        // two entries: `id` (a polymorphic identity function) and `data` (a concrete record).
        // There is no dot-access in Document 1. Document 2 contains a single dict with entry
        // `result`, which accesses `$data.name` (direct field lookup) and calls `[call $id $data.name]`
        // via CALL-POLY. The argument type is resolved through cross-document env lookup.
        //
        // Unlike test_call_poly_state_subst_applied (which uses `\n` in a single document),
        // this test crosses a true document boundary (`---`). The `state` object (including
        // state.subst) is shared across both documents, so any bindings written by document 1
        // are visible to document 2's CALL-POLY return-type resolution.
        //
        // WHY THE DOCUMENT BOUNDARY MATTERS:
        //   After document 1's infer_dict completes (Pass 3b/3c + generalization), the
        //   TypeVar α written into state.subst by check_dot_access is still present as a key
        //   in state.subst.type_map. Document 2 shares this state. If document 2's CALL-POLY
        //   return type (after local-subst resolution) is a TypeVar that is transitively bound
        //   in state.subst from document 1, then the seeded subst in check_call_with_scheme
        //   (which includes state.subst bindings) resolves it via `subst.apply(ret)` at line ~970.
        //
        // CURRENT LIMITATION (tracked as row-unification-f-b in TODO.md):
        //   The CALL-POLY return type in this test resolves correctly through the normal
        //   pipeline (document 1 puts `data` in env as a concrete type; document 2's
        //   dot-access finds `data.name` directly without a state.subst lookup). Thus,
        //   removing `state.subst.apply()` from the CALL-POLY return site ALONE would not
        //   break this test at the current level of constraint propagation.
        //
        //   True isolation — where ONLY removing state.subst.apply() from CALL-POLY causes
        //   a failure — requires that the CALL-POLY return TypeVar (after local subst) be
        //   already bound in state.subst from document 1's dot-access. This is achievable
        //   once cross-field constraint propagation within a single letrec pass is fully
        //   implemented (row-unification-f-b). At that point this comment should be updated
        //   to remove the caveat and the test should tighten to assert exactly that
        //   `state.subst.apply()` at the CALL-POLY site resolves the TypeVar.
        //
        // WHAT THE TEST DOES VERIFY:
        //   - The full CALL-POLY pipeline works across a `---` document boundary
        //   - state.subst is shared across documents (state persists through file_env)
        //   - Document 1's dot-access constraint generation (TypeVar α arm) does not corrupt
        //     state.subst in a way that breaks document 2's CALL-POLY type resolution
        //   - The result is the expected concrete type, not Any or an unresolved TypeVar
        //
        // Document 1: defines `id` (polymorphic identity) and `data` (concrete record).
        //   The letrec for `id: [fn [x@a] $x]` generates a function scheme ∀a. Fn(a→a).
        //   The letrec for `data: [name: hello]` writes `α_data → Record({name: StringLiteral},
        //   Closed)` into the local subst (no state.subst entry from this step).
        //   After document 1, env has `id : ∀a. Fn(a→a)` and `data : Record({name: "hello"})`.
        //   state.subst may have bindings from letrec TypeVar assignments.
        //
        // Document 2: retrieves `id` and `data` from env (concrete, across the `---` boundary),
        //   accesses `$data.name` (direct field lookup, returns StringLiteral("hello")),
        //   then calls `[call $id $data.name]` via CALL-POLY.
        //   CALL-POLY instantiates `id` to Fn(α'→α'), unifies α' with StringLiteral("hello"),
        //   local subst = {α' → StringLiteral("hello")}. subst.apply(α') = StringLiteral("hello").
        //   state.subst.apply(StringLiteral("hello")) = StringLiteral("hello") (no-op on concrete).
        // file_env processes all documents and returns the env of the last document.
        // The last document has one dict [result: ...], so result is in the final env.
        let env = file_env(
            // In new syntax, string literals require quotes.
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n---\n[result: [call $id $data.name]]",
        );
        let result_ty = env
            .get("result")
            .expect("result should be in env after document 2")
            .body
            .clone();
        assert_eq!(
            result_ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY across document boundary should resolve return type to StringLiteral(\"hello\"), got: {result_ty}"
        );
    }

    // -- Type::Any callee positional arg type_map population --

    #[test]
    fn test_call_any_callee_populates_type_map_for_positional_args() {
        // Regression test for the Type::Any arm in check_call and check_call_with_scheme.
        //
        // When the callee resolves to Type::Any (e.g., a variable bound to Any in the env),
        // positional arguments must still be inferred and recorded in type_map — otherwise
        // LSP hover over argument expressions in Any-typed calls produces no type information.
        //
        // The fix (typecheck.rs check_call ~1050, check_call_with_scheme ~900) added an
        // `infer_expr` loop inside the Type::Any arm only. This test guards that loop:
        // if it were removed, the span of `42` would not appear in type_map and the assertion
        // below would fail.
        //
        // SETUP: `f` is bound to TypeScheme::mono(Type::Any) in the parent env, simulating
        // any runtime-typed or externally-typed callable (e.g., a function loaded from JSON,
        // an FFI binding, or a value whose type cannot be statically determined). The call
        // `[call $f 42]` exercises check_call via the monomorphic (empty type_vars) path.
        let input = "[call $f 42]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        // Build a parent env with `f: Any` — monomorphic scheme, empty type_vars.
        let mut parent_env = TypeEnv::new();
        parent_env.insert_scheme("f".to_string(), TypeScheme::mono(Type::Any));
        let parent_env = Rc::new(parent_env);

        let mut state = InferState::new();
        let mut type_map = TypeMap::new();

        let expr = &file.node.documents[0].node.expressions[0];
        let result = infer_expr(expr, &parent_env, &mut state, &mut Some(&mut type_map));

        // The call to an Any-typed function returns Any.
        assert_eq!(
            result,
            Ok(Type::Any),
            "calling Any-typed callee should return Type::Any, got: {result:?}"
        );

        // Extract the span of the `42` argument from the parsed AST to look it up in type_map.
        let arg_span = match &expr.node {
            Expr::Call {
                args, implied: _, ..
            } => {
                assert_eq!(args.len(), 1, "expected exactly one positional arg");
                let arg = &args[0];
                (arg.span.start.offset, arg.span.end.offset)
            }
            other => panic!("expected Expr::Call, got {other:?}"),
        };

        // The span of `42` must appear in type_map: the Type::Any arm must have inferred it.
        assert!(
            type_map.contains_key(&arg_span),
            "type_map should contain the span of `42` (span {arg_span:?}) after calling an Any-typed function, \
             but only found spans: {:?}",
            type_map.keys().collect::<Vec<_>>()
        );

        // The inferred type of `42` should be IntLiteral(42).
        assert_eq!(
            type_map[&arg_span],
            Type::IntLiteral(42),
            "the positional arg `42` should infer to IntLiteral(42), got: {:?}",
            type_map[&arg_span]
        );
    }

    #[test]
    fn test_check_call_mono_subst_apply_documented() {
        // Documents that CALL-MONO in check_call uses state.subst.apply(ret) for defensive
        // consistency (sprint row-unification-h), while check_call_with_scheme (which always
        // takes the CALL-POLY path) has always used it.
        //
        // BACKGROUND: check_call_with_scheme no longer has a CALL-MONO branch. The CALL-MONO
        // branch was deleted (cycle-findings-c36-a Task 2) because it was provably unreachable.
        //
        // CALL-MONO in check_call: uses state.subst.apply(ret) for defensive consistency.
        //
        // WHY check_call CALL-MONO NOW APPLIES state.subst:
        //   check_call applies state.subst.apply(ret) defensively (sprint row-unification-h).
        //   Even though the CALL-MONO guard (!func_ty.has_inference_vars()) proves func_ty is
        //   concrete, applying state.subst ensures consistency with check_call_with_scheme's
        //   CALL-POLY path and guards against future relaxation of the guard (e.g., RowVar-only
        //   polymorphism). The apply() is cheap when state.subst is empty (common case).
        //
        // WHY check_call_with_scheme (CALL-POLY) uses subst.apply(ret):
        //   func_ty comes from instantiate_scheme (line 912), which ALWAYS produces fresh
        //   TypeVars/RowVars. The local subst is seeded from state.subst (mirroring infer_dict
        //   Pass 3a), so subst.apply(ret) resolves both the fresh vars (from argument unification)
        //   and any state.subst-bound vars in a single pass. After the loop, the local subst is
        //   merged back into state.subst (mirroring infer_dict Pass 3d).
        //
        // The test documents the invariant: check_call's CALL-MONO now applies state.subst
        // defensively — both CALL-MONO and CALL-POLY paths call apply() for consistency.

        // Verify current behavior: CALL-MONO in check_call with a monomorphic inline lambda
        let ty = result_field("[f: [fn [x@Int] 42]]\n[result: [call $f 1]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "CALL-MONO should return concrete type"
        );

        // Verify check_call_with_scheme behavior: polymorphic function takes CALL-POLY path
        // (CALL-MONO was deleted from check_call_with_scheme in cycle-findings-c36-a Task 2,
        // since instantiate_scheme always produces fresh TypeVars making CALL-MONO unreachable)
        let ty = result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "check_call_with_scheme CALL-POLY path should unify and apply state.subst"
        );
    }

    #[test]
    fn test_range_access_typevar_target() {
        // Task 4: Coverage test for check_range_access TypeVar fall-through arm.
        //
        // check_range_access has three arms for target_ty:
        //   Type::Record | Type::Any | Type::TypeVar => Ok(target_ty)
        //   _ => Err("expected record type")
        //
        // The TypeVar arm returns Ok(target_ty) without generating constraints (unlike
        // check_dot_access, which generates α = Record({field: β}, RowVar(ρ)) for TypeVar α).
        //
        // This test verifies that range access on a forward-reference does NOT produce a
        // spurious "expected record type" error. The full infer pipeline runs all letrec
        // passes, so by the time infer() returns, the forward-ref TypeVar for $data has been
        // unified with the concrete Record([a: 1  b: 2]). The range access result therefore
        // reflects the resolved target type: a Record (not a TypeVar and not an error).
        //
        // The key invariant: the expression typechecks successfully (returns Ok, not Err).
        // The result type of the range access is the resolved target Record type.

        // Forward-reference range access: $data is a TypeVar during Pass 1/2 of letrec,
        // but by the time infer() returns all passes have completed and the TypeVar is resolved.
        let ty = infer("[result: $data[0..1]  data: [a: 1  b: 2]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                let result_ty = fields.get("result").expect("field 'result' should exist");
                // After full letrec resolution, result is a Record (the resolved target type).
                // This proves: (1) no spurious "expected record type" error during letrec passes,
                // and (2) the TypeVar arm of check_range_access accepted the forward ref cleanly.
                assert!(
                    matches!(result_ty, Type::Record(_)),
                    "range access on forward-ref target should resolve to Record after letrec, got {result_ty}"
                );
            }
            other => panic!("expected Record, got {other}"),
        }

        // TODO: check_range_access should probably generate a constraint like check_dot_access does.
        // Currently it just accepts TypeVar and returns it, meaning range access on an inferred
        // type provides no additional type information. See check_dot_access TypeVar arm
        // for the constraint-generation pattern.
    }

    #[test]
    fn test_range_access_on_proxy_errors() {
        // Range access is NOT supported on Proxy values (unlike dot and bracket access).
        // Runtime eval_range_access returns an error for Value::Proxy (src/eval.rs:1118-1127).
        // Type checker should match this behavior.
        //
        // Note: cannot test via check_err("[p: proxy  x: $p[0..1]]") because within
        // a single dict, $p is still a TypeVar during Pass 3 — the TypeVar arm catches
        // it before the Proxy arm fires. We test check_range_access directly instead.

        let mut file = crate::parse("[dummy: 1][0..1]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        let target = &file.node.documents[0].node.expressions[0];
        let span = target.span;
        let result = check_range_access(target, &None, &None, &env, span, &mut state, &mut None);
        assert!(result.is_ok(), "range access on Record should succeed");

        // Now test with a Proxy target directly by constructing the match input
        let mut proxy_target = crate::parse("[call $proxy [fn [k] 42]]").unwrap();
        crate::desugar::desugar_file(&mut proxy_target.node);
        let proxy_expr = &proxy_target.node.documents[0].node.expressions[0];
        let result =
            check_range_access(proxy_expr, &None, &None, &env, span, &mut state, &mut None);
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("range access is not supported on Proxy")),
            "expected 'range access is not supported on Proxy' error, got: {errors:?}"
        );
    }

    #[test]
    fn test_range_access_on_seq_errors() {
        // Range access is NOT supported on Seq types (sequences are opaque, not dict-like).
        // The Type::Seq arm in check_range_access produces a clear error so callers learn
        // they must use $head/$tail or $collect instead of range slicing.
        //
        // Strategy: bind variable "s" to Type::Seq(Int) in the env, then parse `$s` and
        // call check_range_access directly. infer_expr returns the Seq type from the env,
        // which hits the Type::Seq arm.

        let mut seq_env = TypeEnv::new();
        seq_env.insert("s".to_string(), Type::Seq(Box::new(Type::Int)));
        let env = Rc::new(seq_env);
        let mut state = InferState::new();

        let mut file = crate::parse("$s").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let seq_expr = &file.node.documents[0].node.expressions[0];
        let span = seq_expr.span;

        let result = check_range_access(seq_expr, &None, &None, &env, span, &mut state, &mut None);
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e| e
                .message
                .contains("range access is not supported on Seq types")),
            "expected 'range access is not supported on Seq types' error, got: {errors:?}"
        );
    }

    // -- Variadic param type inference --

    #[test]
    fn test_variadic_param_type_is_any() {
        // Variadic params accept arbitrary fields, typed as Any in the function signature.
        //
        // Grammar: variadic_param = @{ "..." ~ param_name } — no @annotation syntax.
        // The param_types override at infer_fn ensures the function type reflects
        // Any for the variadic slot.

        // Basic variadic: single param, collects all positional args as a dict
        let ty = result_field("[f: [fn [...rest] $rest]]", "f");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(params.len(), 1, "variadic function should have 1 param");
                assert!(
                    matches!(&params[0], Type::Any),
                    "variadic param should have type Any, got: {:?}",
                    params[0]
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }

        // Variadic with annotated params before it: non-variadic params keep their annotation,
        // variadic param is Any regardless
        let ty = result_field("[f: [fn [a@Int b@Int ...rest] $a]]", "f");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(params.len(), 3, "function should have 3 params");
                // First two params have annotation-derived types
                assert!(
                    matches!(&params[0], Type::Int),
                    "annotated param 'a' should be Int, got: {:?}",
                    params[0]
                );
                // Third param (variadic) must be Any
                assert!(
                    matches!(&params[2], Type::Any),
                    "variadic param 'rest' should have type Any, got: {:?}",
                    params[2]
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }
    }

    #[test]
    fn test_variadic_param_env_binding_is_any() {
        // The env binding for a variadic param inside the function body is Any.
        //
        // If the body references $rest, its inferred type comes from the env binding.
        // Returning $rest should give the function an Any return type.

        let ty = result_field("[f: [fn [x ...rest] $rest]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(ret.as_ref(), Type::Any),
                    "function returning variadic param should have Any return type, got: {ret:?}"
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }
    }

    // -- check_call_with_scheme substitution threading (Algorithm W) --

    #[test]
    fn test_call_poly_subst_seeded_and_merged() {
        // Regression test for two Algorithm W substitution threading bugs in
        // check_call_with_scheme (Damas & Milner 1982, Theorem 2):
        //
        //   Task 1 (Critical): The local substitution was never merged back into state.subst.
        //     Bindings accumulated during polymorphic call unification were lost for downstream
        //     inference steps.
        //
        //   Task 2 (Major): The local substitution was not seeded from state.subst.
        //     param_ty was unified against arg_ty in an empty substitution context, missing
        //     bindings for TypeVars that state.subst already resolved.
        //
        // The fix mirrors infer_dict's two-substitution model:
        //   Pass 3a (seed):  initialize local subst from state.subst
        //   Pass 3d (merge): merge local subst back into state.subst
        //
        // TEST SCENARIO (cross-entry):
        //   Entry 1 defines `id : forall a. Fn(a) -> a` and `data : Record({name: "hello"})`.
        //   Entry 2 calls `[call $id $data]` via CALL-POLY.
        //   Entry 3 accesses $result.name.
        //
        //   The cross-entry structure ensures state.subst is the sole channel for
        //   constraint propagation (no infer_dict local subst sharing across entries).
        //   The merge ensures that CALL-POLY's local subst bindings (e.g., _tN -> Record(...))
        //   flow into state.subst for downstream resolution.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n[result: [call $id $data]]\n[n: $result.name]",
            "n",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "cross-entry dot-access on polymorphic call result should resolve, got: {ty}"
        );

        // Also verify that `result` has the full record type.
        // Use a different input where `result` is in the last expression.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n[result: [call $id $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "result should be a record with name: StringLiteral(\"hello\")"
                );
            }
            _ => panic!("expected Record for result, got {ty}"),
        }
    }

    #[test]
    fn test_call_poly_subst_merge_constrains_forward_ref() {
        // Test that check_call_with_scheme's substitution merge propagates constraints
        // from a polymorphic call to forward-referenced letrec entries.
        //
        // SCENARIO: `[fn [x@a y@a] $x]` requires both args to have the same type.
        // When called with `$value` (forward-ref TypeVar) and `42`, the unification
        // binds the forward-ref TypeVar to IntLiteral(42) in the local subst.
        // With the merge, this constraint flows into state.subst.
        //
        // After the letrec processes `value: 42`, the unification of _t_value with
        // IntLiteral(42) in the local subst is consistent with the constraint from
        // the polymorphic call. The result type should be IntLiteral(42).
        let ty = result_field(
            "[same: [fn [x@a y@a] $x]]\n[result: [call $same $value 42]  value: 42]",
            "result",
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "polymorphic call with same-type constraint should resolve return type"
        );

        // Verify `value` also resolves correctly
        let ty = result_field(
            "[same: [fn [x@a y@a] $x]]\n[result: [call $same $value 42]  value: 42]",
            "value",
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "forward-referenced value should have IntLiteral type"
        );
    }

    #[test]
    fn test_call_poly_subst_seed_resolves_access_chain() {
        // Test that check_call_with_scheme's seeded substitution correctly resolves
        // arg_ty through state.subst bindings from prior check_dot_access calls.
        //
        // SCENARIO:
        //   Entry 1: defines `id` (polymorphic) and `data` (concrete record)
        //   Entry 2: defines `name` (accesses $data.name, writes to state.subst)
        //   Entry 3: calls `[call $id $name]` — $name's type should be resolved
        //     through state.subst before unification with the instantiated param type.
        //
        // Without seeding, the fresh local subst would not see state.subst's binding
        // for $name's type. With seeding, unify() resolves both sides through the
        // seeded subst, producing the correct binding.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n[name: $data.name]\n[result: [call $id $name]]",
            "result",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY with access-chain arg should resolve through seeded subst"
        );
    }

    // -- check_call (non-scheme) CALL-POLY substitution threading (Algorithm W) --

    #[test]
    fn test_check_call_nonscheme_poly_subst_seeded_and_merged() {
        // Mirror of test_call_poly_subst_seeded_and_merged for check_call's CALL-POLY path.
        //
        // check_call_with_scheme handles [call $varref ...] when $varref is a polymorphic
        // scheme. check_call handles all other callees, including lambda literals. To trigger
        // check_call's CALL-POLY path, we call a lambda literal directly:
        //   [call [fn [x@a] $x] $data]
        // Since the callee is Expr::Fn (not Expr::VarRef), it routes to check_call (line 263).
        // The lambda infers as Fn(_tN -> _tN) with type vars, so CALL-POLY fires.
        //
        // TEST SCENARIO (merge):
        //   Entry 1: defines `data` as a concrete record.
        //   Entry 2: calls [call [fn [x@a] $x] $data] — CALL-POLY unification binds fresh
        //     TypeVar _tN to Record({name: "hello"}). Without merge, this binding is lost.
        //   Entry 3: accesses $result.name — requires the binding from Entry 2 in state.subst.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[data: [name: \"hello\"]]\n[result: [call [fn [x@a] $x] $data]]\n[n: $result.name]",
            "n",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "check_call CALL-POLY merge: cross-entry dot-access on lambda-call result should resolve"
        );

        // Verify that `result` itself resolves to a record with the right field type.
        let ty = result_field(
            "[data: [name: \"hello\"]]\n[result: [call [fn [x@a] $x] $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "result should be Record with name: StringLiteral(\"hello\")"
                );
            }
            _ => panic!("expected Record for result, got {ty}"),
        }
    }

    #[test]
    fn test_check_call_nonscheme_poly_subst_seed_resolves_access_chain() {
        // Mirror of test_call_poly_subst_seed_resolves_access_chain for check_call's
        // CALL-POLY path.
        //
        // TEST SCENARIO (seed):
        //   Entry 1: defines `data` as a concrete record.
        //   Entry 2: defines `name` via $data.name — check_dot_access writes a constraint
        //     into state.subst binding the TypeVar for $name to StringLiteral("hello").
        //   Entry 3: calls [call [fn [x@a] $x] $name] — the lambda literal callee routes
        //     to check_call (not check_call_with_scheme). CALL-POLY unifies the param type
        //     with arg $name's type. Without seeding from state.subst, the TypeVar for $name
        //     is unresolved during unification.
        //
        // With seeding, the seeded subst resolves $name's TypeVar to StringLiteral("hello")
        // during unification, producing the correct return type.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[data: [name: \"hello\"]]\n[name: $data.name]\n[result: [call [fn [x@a] $x] $name]]",
            "result",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "check_call CALL-POLY seed: access-chain arg should resolve through seeded subst"
        );
    }

    #[test]
    fn test_non_dict_record_preserves_polymorphic_schemes() {
        let input = r#"
            [make-record: [fn [] [id: [fn [x@a] $x]]]]
            ---
            [call $make-record]
            ---
            [result: [call $id 42]]
        "#;

        check(input).expect("should type-check successfully");
    }

    #[test]
    fn test_dict_vs_non_dict_scheme_preservation_parity() {
        let dict_input = r#"
            [id: [fn [x@a] $x]]
            ---
            [result: [call $id 42]]
        "#;

        let non_dict_input = r#"
            [make-record: [fn [] [id: [fn [x@a] $x]]]]
            ---
            [call $make-record]
            ---
            [result: [call $id 42]]
        "#;

        check(dict_input).expect("dict case should type-check");
        check(non_dict_input).expect("non-dict case should type-check");
    }

    // -- Level restoration on error --

    #[test]
    fn test_level_restored_after_non_dict_record_error() {
        // Regression test for level restoration in typecheck_document when infer_expr fails
        // in the Err branch of the non-Dict, non-last expression path in `typecheck_document`.
        //
        // SCENARIO: A multi-document program where a non-last document has a type error.
        // The second document triggers an error (undefined variable `$undefined`), which exercises
        // the Err branch in the non-Dict path in `typecheck_document`, ensuring state.level is
        // correctly restored on error.
        // The third document references a field from the first document - it should still type-check
        // correctly, proving that state.level was restored even though the second document errored.
        //
        // Without level restoration in the Err branch of `typecheck_document`, the third document
        // would inherit the incremented level from the failed second document, causing generalization
        // to fail or produce wrong levels for type variables.
        let input = r#"
            [x: 42]
            ---
            [call $undefined]
            ---
            [result: $x]
        "#;

        // Parse and desugar
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let mut env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        // Process first document (should succeed)
        let doc1 = &file.node.documents[0];
        env = typecheck_document_simple(doc1, &env, &mut state, &mut None)
            .expect("first document should type-check");

        let level_after_doc1 = state.level;

        // Process second document (should fail with undefined variable)
        let doc2 = &file.node.documents[1];
        let result = typecheck_document_simple(doc2, &env, &mut state, &mut None);
        assert!(result.is_err(), "second document should fail");
        assert!(
            result.unwrap_err()[0]
                .message
                .contains("undefined variable"),
            "error should be about undefined variable"
        );

        // CRITICAL: level must be restored after error
        assert_eq!(
            state.level, level_after_doc1,
            "state.level must be restored to enclosing level after error"
        );

        // Process third document (should succeed, proving level was restored)
        let doc3 = &file.node.documents[2];
        env = typecheck_document_simple(doc3, &env, &mut state, &mut None)
            .expect("third document should type-check correctly after level restoration");

        // Verify the result has the correct type
        let result_ty = env.get("result").expect("result should be in env");
        assert_eq!(result_ty.body, Type::IntLiteral(42));
    }

    // -- Malformed composite type annotations --

    #[test]
    fn test_annotation_malformed_function_missing_params() {
        // Regression test for error handling of malformed Fn@ annotations.
        // [Fn@Int] has only 1 entry, but function types require exactly 2.
        let errors = check_err("[fn [f@[type: [Fn@Int]]] $f]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("function type")
                    && e.message.contains("exactly 2 entries")),
            "expected error about function type requiring 2 entries, got: {errors:?}"
        );
    }

    #[test]
    fn test_annotation_malformed_function_non_dict_params() {
        // Function type with non-bracket parameter list should produce clear error.
        // [Fn@Int 42] — second entry is not a bracket expression.
        let errors = check_err("[fn [f@[type: [Fn@Int 42]]] $f]");
        assert!(
            errors.iter().any(|e| e
                .message
                .contains("parameter list must be a bracket expression")),
            "expected error about parameter list, got: {errors:?}"
        );
    }

    #[test]
    fn test_annotation_malformed_nested_record_int_literal() {
        // Nested record type with integer literal instead of type name should produce error.
        // IntLiteral (42) is not a valid type expression.
        let errors = check_err("[fn [p@[type: [outer: [inner: 42]]]] $p]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("invalid type expression in annotation")),
            "expected error about invalid type expression in annotation, got: {errors:?}"
        );
    }

    // -- Open-record subtype rejection --

    #[test]
    fn test_open_record_not_subtype_of_closed() {
        // Companion to open_record_accepts_closed corpus test (positive direction).
        // A function f annotated to accept a CLOSED record [x: Int] cannot be called
        // with an open-record-typed argument: is_subtype(open, closed) = false (Rémy 1994).
        //
        // Uses multi-document input so f's type is fully resolved in document 1 before
        // document 2 type-checks g. Inside g's body, $r has open-record type [x: Int, ...ρ]
        // from its annotation. Passing $r to $f (which expects the closed record [x: Int])
        // triggers the is_subtype guard in check_expr (the synthesize+subsume path) and
        // produces a type mismatch error.
        let errors = check_err(
            "[f: [fn [r@[type: [x: Int]]] $r]]
             ---
             [g: [fn [r@[type: [x: Int ...]]] [call $f $r]]]",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("cannot unify")),
            "expected unification error, got: {errors:?}"
        );
    }

    // -- Arity-mismatch counting (positional + named) --

    #[test]
    fn test_arity_mismatch_shows_counts() {
        // Arity mismatch errors show positional and named arg counts separately.
        //
        // Uses multi-document input so f's type is fully resolved before the call site
        // is checked (avoids letrec TypeVar ambiguity where the function type is not yet
        // concrete when the call is type-checked).
        //
        // [fn [x] $x] takes 1 positional arg; calling with 0 args triggers arity mismatch.
        let errors = check_err(
            "[f: [fn [x] $x]]
             ---
             [result: [call $f]]",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch error, got: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("(0 positional, 0 named)")),
            "expected positional/named counts in arity mismatch error, got: {errors:?}"
        );
    }

    #[test]
    fn test_arity_mismatch_named_args_counted() {
        // Named args count toward arity: [call $f x: 1] with f: [fn [x] $x] has
        // 1 param, 0 positional args, 1 named arg → total_supplied = 1 = params.len() → no error.
        //
        // Uses multi-document input so f's type is fully resolved before the call site.
        // TODO(named-arg-types): Once Type::Function carries param names, this test should
        // additionally verify that the named arg type is unified against the param type.
        let result = check(
            "[f: [fn [x] $x]]
             ---
             [result: [call $f x: 42]]",
        );
        // Named arg `x: 42` fills the one param slot — no arity error expected.
        assert!(
            result.is_ok(),
            "call with named arg filling all param slots should not produce arity error, got: {:?}",
            result.unwrap_err()
        );
    }

    // -- check_call TypeVar arm (letrec forward references) --

    #[test]
    fn test_check_call_forward_ref_function() {
        // Letrec forward reference: $f is called before its definition is inferred.
        // During Pass 3, $f has type TypeVar (from Pass 1). Without the TypeVar arm
        // in check_call, this produces a spurious "expected function type" error.
        // With the fix, check_call returns Any for unbound TypeVar callees.
        let result = check("[result: [call $f 42]  f: [fn [x] $x]]");
        assert!(
            result.is_ok(),
            "forward-reference function call should not produce type error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_check_call_forward_ref_mutual_recursion() {
        // Mutual recursion pattern: $g calls $f which is defined later.
        // Both are forward references during their respective inference passes.
        let result = check("[g: [fn [x] [call $f $x]]  f: [fn [y] $y]]");
        assert!(
            result.is_ok(),
            "mutual forward-reference calls should typecheck, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_check_call_forward_ref_result_type() {
        // The result of calling a forward-referenced function is Any (conservative).
        let ty = result_field("[result: [call $f 42]  f: [fn [x] $x]]", "result");
        // result is Any because $f was a TypeVar when check_call ran
        assert_eq!(ty, Type::Any);
    }

    #[test]
    fn test_check_call_bound_typevar_resolves_to_function() {
        // When state.subst binds a TypeVar to a Function type, applying state.subst
        // before the match resolves the TypeVar so the Function arm fires correctly.
        // Single-document letrec: f and result defined in the same dict so that
        // result_field (which processes documents[0]) can find "result".
        //
        // Note: in a letrec dict, $f is still a TypeVar (the fresh var from Pass 1)
        // when [call $f 42] is processed during Pass 3 value inference. Even after
        // the state.subst.apply() fix, the TypeVar arm fires and returns Any.
        // The apply() call ensures genuinely-resolved TypeVars reach the Function arm,
        // but letrec forward-refs within the same dict remain TypeVars at inference time.
        let ty = result_field("[f: [fn [x] $x]  result: [call $f 42]]", "result");
        assert_eq!(
            ty,
            Type::Any,
            "call to forward-referenced function in same letrec returns Any (TypeVar arm)"
        );
    }

    // -- Pass 3b or_insert unification --

    #[test]
    fn test_pass3b_state_subst_merge_unifies_overlapping_keys() {
        // When state.subst and local subst both bind the same TypeVar (e.g., from
        // an access-chain constraint generated during value inference), the merge
        // should unify the two bindings instead of discarding the state.subst one.
        //
        // Pattern: $data.name generates a constraint in state.subst binding a TypeVar
        // to Record({name: beta}, rho). The local subst from letrec unification also
        // binds the same TypeVar. Without unification, beta remains free.
        //
        // result must come FIRST to create a forward reference — if data comes first,
        // $data is already concrete when result is processed and no collision occurs.
        // In new syntax, string literals require quotes.
        let ty = result_field("[result: $data.name  data: [name: \"hello\"]]", "result");
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "Pass 3b must unify overlapping state.subst binding; got: {ty}"
        );
    }

    // -- Bracket access row constraints --

    #[test]
    fn test_bracket_access_open_record_generates_row_constraint() {
        // When bracket-accessing an open record with a string-literal key not in known
        // fields, check_bracket_access should generate ρ → Row({key: β}, RowVar(ρ'))
        // (mirroring check_dot_access's RowVar arm) instead of returning Type::Any.
        //
        // Pattern: [Open: [type [name: String ...]]]
        //          [p: [@Open [name: Alice  score: 42]]]
        //          [r: $p["score2"]]
        // r should be a TypeVar (fresh β from the constraint), not Any.
        // In new syntax, string literals require quotes (both value and bracket key).
        let env = doc_env(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  score: 42]]]\n[r: $p[\"score2\"]]",
        );
        match env.get("r").map(|s| &s.body) {
            Some(Type::TypeVar(_, _)) => {}
            Some(other) => panic!(
                "expected TypeVar for bracket access on open record unknown field, got {other}"
            ),
            None => panic!("field 'r' not found in env"),
        }
    }

    #[test]
    fn test_bracket_access_open_record_known_field() {
        // Bracket access with a string-literal key that IS in known fields should return
        // the field's type directly, unchanged from previous behavior.
        // In new syntax, string literals require quotes (both value and bracket key).
        let env = doc_env(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"]]]\n[r: $p[\"name\"]]",
        );
        match env.get("r").map(|s| &s.body) {
            Some(Type::Str) => {}
            Some(other) => panic!("expected Str for bracket access on known field, got {other}"),
            None => panic!("field 'r' not found in env"),
        }
    }

    #[test]
    fn test_bracket_access_closed_record_missing_field_errors() {
        // Bracket access with a string-literal key not in fields of a closed record should
        // error, not return Any.
        let result = check("[p: [name: Alice]]\n[r: $p[unknown]]");
        assert!(
            result.is_err(),
            "bracket access on closed record for missing field should error"
        );
    }

    #[test]
    fn test_bracket_access_typevar_generates_constraint() {
        // When target is a TypeVar and key is a string literal, check_bracket_access should
        // generate α = Record({key: β}, RowVar(ρ)) (mirroring check_dot_access's TypeVar arm).
        //
        // Pattern: [result: $data["name"]  data: [name: "hello"]]
        // Pass 3b resolves β through the γ_data collision → StringLiteral("hello").
        // In new syntax, string literals require quotes (both value and bracket key).
        let ty = result_field(
            "[result: $data[\"name\"]  data: [name: \"hello\"]]",
            "result",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "bracket access on TypeVar must generate constraint resolved by Pass 3b; got {ty}"
        );
    }

    #[test]
    fn test_bracket_access_typevar_dynamic_key_returns_any() {
        // When target is a TypeVar and key is also a TypeVar (dynamic, non-literal),
        // check_bracket_access cannot generate field-level constraints and returns Any.
        //
        // [fn [data key] $data[$key]] — both `data` and `key` are fresh TypeVars (no
        // annotations). The key TypeVar is not a literal, so static_field = None, falling
        // to the Type::Any | Type::TypeVar arm in check_bracket_access which returns Any.
        let fn_ty = infer("[fn [data key] $data[$key]]");
        match fn_ty {
            Type::Function { ret, .. } => {
                assert_eq!(
                    *ret,
                    Type::Any,
                    "bracket access on TypeVar with dynamic TypeVar key must return Any; got {ret}"
                );
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    // -- resolve_type_assert state.subst.apply() regression --

    #[test]
    fn test_resolve_type_assert_subst_apply_is_load_bearing() {
        // Regression test for `state.subst.apply(&expected)` at the end of resolve_type_assert.
        //
        // The apply at line ~1482 ensures that TypeVars inside `expected` are resolved through
        // the current substitution before the type is returned and recorded in the AST node.
        // Without the apply, a TypeVar that was bound in state.subst during check_expr (or
        // during a prior inference step in the same letrec pass) would remain unresolved in
        // the returned type, causing downstream inference to see an unresolved TypeVar where
        // a concrete type was expected.
        //
        // ISOLATION SCENARIO:
        // The scenario where ONLY removing state.subst.apply(&expected) causes a failure
        // requires that `expected` contains a TypeVar bound in state.subst. Since
        // resolve_type_assert calls resolve_annotation with &mut None (no ann_mapping),
        // a lowercase annotation name like `@a` produces TypeVar("a", level) as expected.
        //
        // For TypeVar("a") to be in state.subst, something in the letrec pass before or
        // during check_expr must unify "a" with a concrete type. The current architecture
        // does not produce this naturally (check_expr synthesizes + checks is_subtype,
        // never calling unify with the expected TypeVar as an argument).
        //
        // A full isolation test requires cross-field constraint propagation within a letrec
        // pass (tracked as future work in row-unification-h). This test instead verifies:
        //   (a) TypeAssert with a concrete expected type returns the expected type (not the
        //       inner expression's more specific type — TypeAssert widens to the annotation)
        //   (b) state.subst.apply() on a concrete type is a no-op (idempotence)
        //   (c) The apply path does not break the return value
        //
        // WHAT WOULD BREAK WITHOUT THE APPLY:
        // If `expected` is TypeVar("a") and "a" were bound to Int in state.subst:
        //   - Without apply: resolve_type_assert returns TypeVar("a"), which later appears
        //     in the type_map and env as an unresolved TypeVar.
        //   - With apply: resolve_type_assert returns Int, which is the concrete resolved type.
        //
        // The `resolved_type` RefCell is stored AFTER state.subst.apply(), so both the runtime
        // elaboration and static type checking see the same fully-resolved post-apply type.

        // Case 1: TypeAssert with Int annotation returns Int (not IntLiteral(42))
        // This verifies the apply path returns the expected type (widening behavior).
        // Without apply (for concrete types), result is identical — but this exercises the code path.
        let ty = result_field("[x: [@Int 42]]", "x");
        assert_eq!(
            ty,
            Type::Int,
            "[@Int 42] should return Int (the asserted type), not IntLiteral(42)"
        );

        // Case 2: TypeAssert with default: — inner fails, default succeeds.
        // Tests that state.subst.apply(&expected) at line ~1461 (default check path)
        // resolves the expected type correctly.
        // [@[type: Int  default: 42] $missing]: $missing is undefined, check_expr fails,
        // default 42 is inferred as IntLiteral(42), is_subtype(IntLiteral, Int) = true,
        // return apply(Int) = Int.
        let ty = result_field("[x: [@[type: Int  default: 42] $missing]]", "x");
        assert_eq!(
            ty,
            Type::Int,
            "[@[type: Int  default: 42] $missing] should return Int (the asserted type) using the default"
        );

        // Case 3: Verify the apply at line ~1482 works for a concrete annotation type.
        // [@[type: [x: Int  y: Int]] [x: 1  y: 2]]: check_expr on the inner record against
        // Record{x: Int, y: Int}. is_subtype passes (IntLiteral(1) <: Int).
        // state.subst.apply(Record{x: Int, y: Int}) = Record{x: Int, y: Int} (no-op on concrete).
        // The apply is idempotent — this guards against regression where apply corrupts concrete types.
        let ty = result_field("[p: [@[type: [x: Int  y: Int]] [x: 1  y: 2]]]", "p");
        match ty {
            Type::Record(Row { ref fields, ref tail }) => {
                assert_eq!(
                    fields.get("x"),
                    Some(&Type::Int),
                    "record.x should be Int"
                );
                assert_eq!(
                    fields.get("y"),
                    Some(&Type::Int),
                    "record.y should be Int"
                );
                assert_eq!(
                    *tail,
                    RowTail::Empty,
                    "type-asserted record should be closed"
                );
            }
            other => panic!(
                "[@[type: [x: Int  y: Int]] [x: 1  y: 2]] should return the annotated record type, got {other}"
            ),
        }
    }

    // -- check_call_with_scheme func span recording --

    #[test]
    fn test_check_call_with_scheme_records_func_span_in_type_map() {
        // Regression test for func span recording in check_call_with_scheme.
        //
        // When a polymorphic function is called via VarRef, infer_expr routes to
        // check_call_with_scheme (to avoid double instantiation). Because this path
        // bypasses infer_expr for the function expression, the function VarRef span
        // would NOT appear in type_map unless check_call_with_scheme records it explicitly.
        //
        // This test verifies that after check_call_with_scheme runs, type_map contains
        // an entry for the function name's span with the instantiated function type.
        // This is required for LSP hover to show the type of the function name at the
        // call site (e.g., hovering over `$id` in `[call $id 42]` shows `Fn(Int → Int)`).
        //
        // check_call (the non-scheme path) records the func span automatically via
        // infer_expr(func, ...) which populates type_map on every infer_expr call.
        // check_call_with_scheme must mirror this behavior by recording explicitly.
        //
        // SETUP: A polymorphic identity function `id` in a separate document (so it is
        // fully generalized and the call routes to check_call_with_scheme, not check_call).
        let input = "[id: [fn [x@a] $x]]\n---\n[result: [call $id 42]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let mut env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let mut type_map = TypeMap::new();

        // Process document 1 (defines `id`)
        env = typecheck_document_simple(
            &file.node.documents[0],
            &env,
            &mut state,
            &mut Some(&mut type_map),
        )
        .expect("document 1 should type-check");

        // Process document 2 (calls `$id`)
        env = typecheck_document_simple(
            &file.node.documents[1],
            &env,
            &mut state,
            &mut Some(&mut type_map),
        )
        .expect("document 2 should type-check");

        // Verify result resolves to IntLiteral(42) (correct CALL-POLY behavior)
        let result_ty = env
            .get("result")
            .expect("result should be in env")
            .body
            .clone();
        assert_eq!(
            result_ty,
            Type::IntLiteral(42),
            "CALL-POLY should return the argument type via identity function"
        );

        // Find the span of `$id` in `[result: [call $id 42]]` from the second document.
        // The outer expression in document 2 is a Dict [result: [call $id 42]].
        // We need to dig into the dict entry's value to find the Call expression.
        let doc2_expr = &file.node.documents[1].node.expressions[0];
        let func_span = match &doc2_expr.node {
            Expr::Dict(entries) => {
                // Find the entry with key "result"
                let call_entry = entries
                    .iter()
                    .find(|e| {
                        matches!(&e.node.key, Some(k) if matches!(&k.node, Expr::Str(s) if s == "result"))
                    })
                    .expect("should have 'result' entry");
                match &call_entry.node.value.node {
                    Expr::Call {
                        func, implied: _, ..
                    } => (func.span.start.offset, func.span.end.offset),
                    other => {
                        panic!("expected Expr::Call as value of 'result' entry, got {other:?}")
                    }
                }
            }
            Expr::Call { func, .. } => (func.span.start.offset, func.span.end.offset),
            other => panic!("expected Expr::Dict or Expr::Call in document 2, got {other:?}"),
        };

        // The func span ($id) must appear in type_map.
        assert!(
            type_map.contains_key(&func_span),
            "type_map must contain the span of `$id` (the polymorphic function VarRef) \
             after check_call_with_scheme — required for LSP hover. \
             func span: {func_span:?}, type_map keys: {:?}",
            type_map.keys().collect::<Vec<_>>()
        );

        // The type recorded for `$id` should be the instantiated function type
        // (a Function type, since id was called with an Int arg — instantiated to Fn(Int→Int)).
        let recorded_ty = &type_map[&func_span];
        assert!(
            matches!(recorded_ty, Type::Function { .. }),
            "type_map entry for `$id` should be a Function type (instantiated scheme), got {recorded_ty}"
        );
    }

    // -- check_expr lambda arity mismatch --

    #[test]
    fn test_check_expr_lambda_arity_mismatch() {
        // Lambda with 2 params checked against a Fn type expecting 1 param triggers the
        // arity check inside check_expr's lambda checking mode (lines 433-442).
        //
        // We call check_expr directly with a hand-built AST to avoid the @[...] composite
        // annotation syntax which is not yet implemented in the parser. This tests the
        // actual arity check code path without going through the full parse pipeline.
        let span = Span::origin();

        // Build: [fn [x y] $x] — a 2-param lambda
        let param_x = Spanned::new(
            Param {
                name: "x".to_string(),
                annotation: None,
                variadic: false,
            },
            span,
        );
        let param_y = Spanned::new(
            Param {
                name: "y".to_string(),
                annotation: None,
                variadic: false,
            },
            span,
        );
        let body = Spanned::new(Expr::VarRef("x".to_string()), span);
        let lambda = Spanned::new(
            Expr::Fn {
                return_ann: None,
                params: vec![param_x, param_y],
                body: Rc::new(body),
                desugared: false,
            },
            span,
        );

        // Expected type: Fn(String -> Int) — a 1-param function type
        let expected_ty = Type::Function {
            params: vec![Type::Str],
            ret: Box::new(Type::Int),
            variadic: false,
        };

        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let result = check_expr(&lambda, &expected_ty, &env, &mut state, &mut None);

        assert!(
            result.is_err(),
            "Lambda with 2 params checked against 1-param Fn type should error"
        );
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "Expected arity mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_double_typecheck_no_panic() {
        // Regression test for LSP double-typecheck panic risk.
        // Before the fix, calling typecheck_file_with_types twice on the same AST
        // would trigger the write-once invariant assertion in resolve_type_assert.
        // After the fix, reset_elaboration clears resolved_type fields before each typecheck.
        let input = r#"
            [@Number 42]
            [@String "hello"]
            [@Number 99]
        "#;

        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        // First typecheck: should succeed
        let (errors1, type_map1) = typecheck_file_with_types(&file.node);
        assert!(
            errors1.is_empty() || errors1.iter().all(|e| !e.message.contains("panic")),
            "First typecheck should not panic"
        );
        assert!(
            !type_map1.is_empty(),
            "First typecheck should populate type_map"
        );

        // Second typecheck on the same AST: should not panic due to reset_elaboration
        let (errors2, type_map2) = typecheck_file_with_types(&file.node);
        assert!(
            errors2.is_empty() || errors2.iter().all(|e| !e.message.contains("panic")),
            "Second typecheck should not panic"
        );
        assert!(
            !type_map2.is_empty(),
            "Second typecheck should populate type_map"
        );

        // Third typecheck to be extra sure
        let (errors3, _type_map3) = typecheck_file_with_types(&file.node);
        assert!(
            errors3.is_empty() || errors3.iter().all(|e| !e.message.contains("panic")),
            "Third typecheck should not panic"
        );
    }

    // -- Type::Error cascade prevention --

    #[test]
    fn test_error_recorded_in_type_map_on_failure() {
        // When infer_expr fails on a sub-expression, Type::Error must be recorded in the
        // type_map for LSP hover so the parent expression sees <error> rather than nothing.
        //
        // Test via typecheck_file_with_types: $undefined is a VarRef that fails, so the
        // type_map entry for its span must be Type::Error.
        let input = "$undefined";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, type_map) = typecheck_file_with_types(&file.node);

        // Must have an error (undefined variable)
        assert!(!errors.is_empty(), "expected type error for $undefined");

        // The type_map should contain at least one Type::Error entry
        let has_error = type_map.values().any(|ty| matches!(ty, Type::Error));
        assert!(
            has_error,
            "type_map should contain Type::Error for failed sub-expression ($undefined), \
             got entries: {:?}",
            type_map.values().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_cascade_prevention_error_does_not_multiply_errors() {
        // Cascade prevention: when a call argument fails inference, only the original
        // error should be reported — not a cascade of "wrong argument type" errors on top.
        //
        // [f: [fn [x@Int] $x]] called with $undefined (an undefined variable).
        // Without cascade prevention: two errors — (1) undefined variable, (2) arg type mismatch.
        // With cascade prevention: only one error — undefined variable.
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [call $f $undefined]]");

        // Must have at least one error
        assert!(!errors.is_empty(), "expected at least one type error");

        // The error should be about the undefined variable
        let has_undefined_err = errors
            .iter()
            .any(|e| e.message.contains("undefined variable"));
        assert!(
            has_undefined_err,
            "expected undefined variable error, got: {:?}",
            errors
        );

        // Should NOT have a spurious "cannot unify" error about Int vs the arg type.
        // The Error sentinel absorbs the param type without generating a new mismatch.
        let has_cascade_err = errors
            .iter()
            .any(|e| e.message.contains("cannot unify") && e.message.contains("Int"));
        assert!(
            !has_cascade_err,
            "cascade error about Int unification should be suppressed by Error absorption, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_error_absorbed_in_unify_does_not_corrupt_substitution() {
        // Verifies that unify(Error, TypeVar) does not bind the TypeVar, which would corrupt
        // subsequent inference. After cascade prevention records Error as an arg type, the
        // unification step must absorb it without touching the substitution.
        //
        // If Error were to bind a TypeVar (e.g., _t0 ↦ Error), the return type of the
        // polymorphic call would resolve to Error, suppressing valid type information
        // for the surrounding context.
        let span = Span::origin();
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);

        // Simulate: polymorphic param type is TypeVar("a"), arg type is Error
        let result = unify(
            &Type::TypeVar("a".into(), 1),
            &Type::Error,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_ok(), "unify(TypeVar, Error) must succeed");
        assert!(
            subst.type_map.is_empty(),
            "TypeVar must NOT be bound when unified with Error (Error carries no type info)"
        );
    }

    // -- check_call_with_scheme error paths --

    #[test]
    fn test_check_call_with_scheme_arity_mismatch() {
        // Arity mismatch when calling a polymorphic scheme with wrong number of args.
        // The scheme has 2 params but we provide 1 positional arg → arity mismatch error.
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch error when calling polymorphic scheme, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_check_call_with_scheme_non_function_error() {
        // Calling a non-function scheme (type is Int, not Function).
        // check_call_with_scheme should produce "expected function type" error.
        let errors = check_err("[x: 42]\n---\n[result: [call $x 1 2]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expected function type")),
            "expected 'expected function type' error when calling Int scheme, got: {:?}",
            errors
        );
    }

    // -- Builtin sequence types --

    #[test]
    fn test_builtin_seq_generators_return_seq_types() {
        // Regression test for type-seq sprint: sequence-generating builtins should return Type::Seq.
        // Covers: $seq, $repeat, $cycle, $iterate, $unfold, $take
        // NOTE: $seq takes (head, tail) args — it's the primitive Seq cons operation
        let input = r#"
            [some_seq: [call $range 0 10]]
            [seq_result: [call $seq 1 $some_seq]]
            [repeat_result: [call $repeat 42]]
            [cycle_result: [call $cycle $some_seq]]
            [iterate_result: [call $iterate [fn [x@a] $x] 0]]
            [unfold_result: [call $unfold [fn [x@a] [Just: [x  $x]]] 0]]
            [take_result: [call $take 5 $some_seq]]
        "#;
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();
        let new_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .expect("typecheck should succeed");

        // $seq should return Seq(Int) — all args are IntLiterals
        let seq_ty = new_env.get("seq_result").unwrap().body.clone();
        match seq_ty {
            Type::Seq(_) => {} // success
            other => panic!("seq should return Seq, got: {other}"),
        }

        // $repeat should return Seq(Int)
        let repeat_ty = new_env.get("repeat_result").unwrap().body.clone();
        match repeat_ty {
            Type::Seq(_) => {} // success
            other => panic!("repeat should return Seq, got: {other}"),
        }

        // $cycle should return Seq
        let cycle_ty = new_env.get("cycle_result").unwrap().body.clone();
        match cycle_ty {
            Type::Seq(_) => {} // success
            other => panic!("cycle should return Seq, got: {other}"),
        }

        // $iterate should return Seq
        let iterate_ty = new_env.get("iterate_result").unwrap().body.clone();
        match iterate_ty {
            Type::Seq(_) => {} // success
            other => panic!("iterate should return Seq, got: {other}"),
        }

        // $unfold should return Seq
        let unfold_ty = new_env.get("unfold_result").unwrap().body.clone();
        match unfold_ty {
            Type::Seq(_) => {} // success
            other => panic!("unfold should return Seq, got: {other}"),
        }

        // $take should return Seq
        let take_ty = new_env.get("take_result").unwrap().body.clone();
        match take_ty {
            Type::Seq(_) => {} // success
            other => panic!("take should return Seq, got: {other}"),
        }
    }

    // -- merge/append RowVar regression --

    #[test]
    fn test_merge_no_rowvar_sharing_error() {
        // Regression test: merge [a: 1] [b: 2] should type-check without error.
        // Previous RowVar sharing bug would fail because the same row var appeared
        // in both params and return type of the builtin signature.
        let result = check("[result: [merge [a: 1] [b: 2]]]");
        assert!(
            result.is_ok(),
            "merge with simple records should type-check, got error: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_append_no_rowvar_sharing_error() {
        // Regression test: append [a: 1] [b: 2] should type-check without error.
        // Previous RowVar sharing bug would fail because the same row var appeared
        // in both param and return type of the builtin signature.
        let result = check("[result: [append [a: 1] [b: 2]]]");
        assert!(
            result.is_ok(),
            "append with simple records should type-check, got error: {:?}",
            result.unwrap_err()
        );
    }

    // -- % pipeline variable binding --

    #[test]
    fn test_pipeline_percent_binding() {
        // Test that % is bound to the pipeline type in each document
        let input = r#"
[x: 1  y: 2]

---

[z: [+ %.x %.y]]
        "#;
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let result = typecheck_file(&file.node);
        assert!(
            result.is_ok(),
            "% pipeline binding should work, got error: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_pipeline_percent_pipeline_multi_field() {
        // Test that the inferred type of z ([+ %.x %.y]) is Number (the + return type).
        // Uses file_env_with_builtins because + is a stdlib builtin (not in TypeEnv::new()).
        let input = "[x: 1  y: 2]\n---\n[z: [+ %.x %.y]]";
        let env = file_env_with_builtins(input);
        let result_type = env.get("%").unwrap().body.clone();
        match result_type {
            Type::Record(Row { fields, .. }) => {
                let z = fields
                    .get("z")
                    .expect("field 'z' should exist in second doc");
                assert_eq!(
                    *z,
                    Type::Number,
                    "expected [+ %.x %.y] to have type Number, got {z}"
                );
            }
            other => panic!("expected Record result for second doc, got {other}"),
        }
    }

    #[test]
    fn test_named_section_binding() {
        // Test that named sections bind as %name
        let input = r#"
--- %data
[x: 1  y: 2]

---

[z: [+ %data.x %data.y]]
        "#;
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let result = typecheck_file(&file.node);
        assert!(
            result.is_ok(),
            "named section binding should work, got error: {:?}",
            result.unwrap_err()
        );
    }
}
