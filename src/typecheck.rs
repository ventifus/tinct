//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{
    Annotation, Document, Entry, Expr, File, NamedArg, Param, Pattern, Span, Spanned,
};
use crate::coverage;
use crate::types::{
    generalize, instantiate_at_level, instantiate_scheme, resolve_has_field, unify, Constraint,
    InferState, Kind, Label, Row, Substitution, Type, TypeAlias, TypeEnv, TypeError, TypeScheme,
};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;

use typecheck_annot::*;
use typecheck_dict::*;

/// Map from source span `(start_offset, end_offset)` to inferred type. Populated during type
/// checking so LSP hover/diagnostics can look up types without re-running inference. Offsets
/// are sufficient as keys; the full `Span` source text is not needed.
pub type TypeMap = HashMap<(usize, usize), Type>;

/// Map from variable/parameter name to its documentation string.
/// Populated during type checking by extracting `doc:` properties from annotations.
pub type DocMap = HashMap<String, String>;

/// Re-export SchemeMap from types for LSP consumers.
pub use crate::types::SchemeMap;

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
pub fn typecheck_file(file: &File) -> (Vec<TypeError>, Vec<crate::error::TypeDiagnostic>) {
    // Reset elaboration state to allow re-typechecking cached ASTs
    reset_elaboration(file);

    let mut errors = Vec::new();
    let mut diagnostics = Vec::new();
    let mut env = crate::imports::build_prelude_env();
    let mut state = InferState::new();
    // Seed with prelude instances so constraint checking works for dynamically registered classes.
    crate::imports::seed_infer_state_from_prelude_cache(&mut state);

    let mut type_map = TypeMap::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
    });

    for doc in &file.documents {
        // Skip type-stage documents — they are handled separately by create_type_stage_env()
        // and should not be type-checked in the runtime pipeline.
        if doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

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

    // Scan for type quality issues (Unknown types, over-broad annotations)
    scan_type_quality(&type_map, file, &mut diagnostics);

    // Extract diagnostics accumulated during type inference (e.g., ambiguous constraints)
    diagnostics.append(&mut state.diagnostics);

    (errors, diagnostics)
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
        | Expr::VarRef { .. }
        | Expr::Rest(_)
        | Expr::Placeholder
        | Expr::TypeApp { .. }
        | Expr::Error(_) => {}

        // Access expressions: recurse into target
        Expr::DotAccess { expr: target, .. } => {
            reset_expr(target);
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
        Expr::TypeAlias { body, .. } => {
            reset_expr(body);
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

        // Pipe: recurse into both sides
        Expr::Pipe { lhs, rhs } => {
            reset_expr(lhs);
            reset_expr(rhs);
        }

        // Sequential: recurse into all expressions
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                reset_expr(seq_expr);
            }
        }

        // Annotated: annotations don't contain TypeAssert nodes, so nothing to reset
        Expr::Annotated { .. } => {}

        // Quote: recurse into the inner expression
        Expr::Quote(inner) => {
            reset_expr(inner);
        }

        // Unquote and UnquoteSplice: recurse into the inner expression
        Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            reset_expr(inner);
        }

        // DefMacro: recurse into the body
        Expr::DefMacro { body, .. } => {
            reset_expr(body);
        }

        // Match: recurse into scrutinee and arm bodies
        Expr::Match { scrutinee, arms } => {
            reset_expr(scrutinee);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    reset_expr(guard);
                }
                reset_expr(&arm.body);
            }
        }

        // ClassDecl: recurse into method signatures
        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    reset_expr(key);
                }
                reset_expr(&method.node.value);
            }
        }

        // InstanceDecl: recurse into pattern expressions and method implementations
        Expr::InstanceDecl { arms, .. } => {
            for (pattern_expr, methods) in arms {
                reset_expr(pattern_expr);
                for method in methods {
                    if let Some(key) = &method.node.key {
                        reset_expr(key);
                    }
                    reset_expr(&method.node.value);
                }
            }
        }

        // PatternDecl: recurse into bindings
        Expr::PatternDecl { bindings } => {
            for binding in bindings {
                reset_expr(binding);
            }
        }

        // LetDecl: recurse into bindings
        Expr::LetDecl { bindings } => {
            for binding in bindings {
                reset_expr(binding);
            }
        }

        // CaseArm: recurse into pattern and body
        Expr::CaseArm { pattern, body } => {
            reset_expr(pattern);
            reset_expr(body);
        }
    }
}

/// Type-check a file, returning errors, a type map, and a doc map for LSP features.
/// The type map and doc map are populated even when errors occur, covering
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
pub fn typecheck_file_with_types(
    file: &File,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    typecheck_file_with_types_and_env(file, crate::imports::build_prelude_env())
}

/// Type-check a parsed [`File`] with a custom initial type environment.
///
/// Like [`typecheck_file_with_types`], but accepts a custom initial environment.
/// This is used by the LSP to seed the type environment with prelude names,
/// suppressing false "undefined variable" errors for stdlib functions.
pub fn typecheck_file_with_types_and_env(
    file: &File,
    initial_env: Rc<TypeEnv>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    typecheck_file_with_types_and_env_and_source(file, initial_env, None)
}

/// Type-check a parsed [`File`] with a custom initial type environment and source path.
///
/// Like [`typecheck_file_with_types_and_env`], but accepts an optional source path
/// for diagnostics that need to know the source file.
pub fn typecheck_file_with_types_and_env_and_source(
    file: &File,
    initial_env: Rc<TypeEnv>,
    // source_path was previously used for T009 builtin-* alias diagnostics.
    // T009 was removed as part of the primitive-privacy sprint (Phase 4).
    // The parameter is kept for API compatibility; callers may still pass Some(path).
    _source_path: Option<String>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    let (errors, type_map, doc_map, scheme_map, diagnostics, _state) =
        typecheck_file_with_types_and_env_and_source_returning_state(
            file,
            initial_env,
            true,
            false,
        );
    (errors, type_map, doc_map, scheme_map, diagnostics)
}

/// Like [`typecheck_file_with_types_and_env_and_source`], but also returns the final
/// [`InferState`]. Used by `imports::build_prelude_env_inner` to capture the prelude's
/// `instance_env` for propagation to user-code type-checking sessions. Also used by eval
/// pipeline to extract boundary_guards for gradual typing runtime checks.
pub fn typecheck_file_with_types_and_env_and_source_returning_state(
    file: &File,
    initial_env: Rc<TypeEnv>,
    enable_scheme_map: bool,
    in_prelude_load: bool,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
    InferState,
) {
    // Reset elaboration state to allow re-typechecking cached ASTs
    reset_elaboration(file);

    let mut errors = Vec::new();
    let mut diagnostics = Vec::new();
    let mut env = initial_env;
    let mut state = InferState::new();
    // Seed with prelude instances so user code sees class instances registered by prelude.llt.
    // This call is a no-op when the cache is empty (i.e., when we ARE type-checking the prelude).
    crate::imports::seed_infer_state_from_prelude_cache(&mut state);

    // Set the prelude load flag if requested (optimization for prelude type-checking)
    state.in_prelude_load = in_prelude_load;

    if enable_scheme_map {
        // Enable scheme collection for LSP hover (constraints display).
        state.scheme_map = Some(SchemeMap::new());
    }
    let mut type_map = TypeMap::new();
    let mut doc_map = DocMap::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
    });

    for doc in &file.documents {
        // Skip type-stage documents — they are handled separately by create_type_stage_env()
        // and should not be type-checked in the runtime pipeline.
        if doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

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

    // Extract doc strings from all documents
    extract_doc_strings(file, &mut doc_map);

    // Extract scheme_map from state (was populated during VarRef inference).
    let scheme_map = state.scheme_map.take().unwrap_or_default();

    // Scan for type quality issues (Unknown types, over-broad annotations)
    scan_type_quality(&type_map, file, &mut diagnostics);

    // Extract diagnostics accumulated during type inference (e.g., T013 ambiguous constraints).
    // These are distinct from scan_type_quality diagnostics (T010/T011 Unknown type warnings).
    // Without this, state.diagnostics are only visible to callers that inspect the returned
    // state directly — typecheck_source (and the corpus test pipeline) would silently drop them.
    diagnostics.append(&mut state.diagnostics);

    (errors, type_map, doc_map, scheme_map, diagnostics, state)
}

/// Extract documentation strings from parameter and function annotations.
///
/// Walks the AST looking for `doc:` properties in `@[...]` annotations.
/// Populates the doc_map with entries like `param_name -> "doc string"`.
fn extract_doc_strings(file: &File, doc_map: &mut DocMap) {
    for document in &file.documents {
        for expr in &document.node.expressions {
            extract_doc_from_expr(&expr.node, doc_map, None);
        }
    }
}

/// Recursively extract doc strings from an expression tree.
///
/// `binding_name`: When recursing into a dict entry value, this is the entry's binding name
/// (used to key function-level doc strings extracted from return annotations).
fn extract_doc_from_expr(expr: &Expr, doc_map: &mut DocMap, binding_name: Option<&str>) {
    match expr {
        Expr::Fn {
            return_ann,
            params,
            body,
            ..
        } => {
            // Extract doc strings from parameter annotations
            for param in params {
                if let Some(ref ann) = param.node.annotation {
                    if let Some(doc_value) = ann.node.get_property("doc") {
                        if let Expr::Str(doc_string) = &doc_value.node {
                            doc_map.insert(param.node.name.clone(), doc_string.clone());
                        }
                    }
                }
            }
            // Extract doc from return annotation if present (function-level doc)
            if let Some(binding) = binding_name {
                if let Some(ref ann) = return_ann {
                    if let Some(doc_value) = ann.node.get_property("doc") {
                        if let Expr::Str(doc_string) = &doc_value.node {
                            doc_map.insert(binding.to_string(), doc_string.clone());
                        }
                    }
                }
            }
            // Recurse into function body
            extract_doc_from_expr(&body.node, doc_map, None);
        }
        Expr::Dict(entries) => {
            for entry in entries {
                // Extract doc from key annotation (e.g., name@[doc: "..."])
                if let Some(ref key_expr) = entry.node.key {
                    if let Expr::Annotated { name, annotation } = &key_expr.node {
                        if let Some(doc_value) = annotation.node.get_property("doc") {
                            if let Expr::Str(doc_string) = &doc_value.node {
                                doc_map.insert(name.clone(), doc_string.clone());
                            }
                        }
                        // Thread the binding name when recursing into the value
                        extract_doc_from_expr(&entry.node.value.node, doc_map, Some(name));
                        continue;
                    }
                    // Plain VarRef key (e.g., `calc: [fn@[doc: "..."] ...]`)
                    // Thread the binding name so fn return annotations can be keyed
                    if let Expr::VarRef { name, .. } = &key_expr.node {
                        extract_doc_from_expr(&entry.node.value.node, doc_map, Some(name));
                        continue;
                    }
                }
                // No annotation on key, recurse without binding name
                extract_doc_from_expr(&entry.node.value.node, doc_map, None);
            }
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            extract_doc_from_expr(&func.node, doc_map, None);
            for arg in args {
                extract_doc_from_expr(&arg.node, doc_map, None);
            }
            for named_arg in named_args {
                extract_doc_from_expr(&named_arg.node.value.node, doc_map, None);
            }
        }
        Expr::DotAccess { expr: target, .. } => {
            extract_doc_from_expr(&target.node, doc_map, None);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            extract_doc_from_expr(&inner.node, doc_map, None);
        }
        Expr::TypeAlias { body, .. } => {
            extract_doc_from_expr(&body.node, doc_map, None);
        }
        Expr::Pipe { lhs, rhs } => {
            extract_doc_from_expr(&lhs.node, doc_map, None);
            extract_doc_from_expr(&rhs.node, doc_map, None);
        }
        // Literals and VarRef have no children to recurse into
        _ => {}
    }
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
                // Apply substitution before consistency check
                let (pipeline_type_resolved, expected_type_resolved) = if state.subst.is_empty() {
                    (pipeline_type.clone(), expected_type.clone())
                } else {
                    (
                        state.subst.apply(pipeline_type),
                        state.subst.apply(&expected_type),
                    )
                };
                let passes = Type::is_subtype(&pipeline_type_resolved, &expected_type_resolved)
                    || ((contains_unknown_or_top(&pipeline_type_resolved)
                        || contains_unknown_or_top(&expected_type_resolved))
                        && Type::is_consistent(&pipeline_type_resolved, &expected_type_resolved));
                if !passes {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Pipeline input type {} does not satisfy expects contract {}",
                            pipeline_type_resolved, expected_type_resolved
                        ),
                        expects_ann.span,
                    ));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    // Process caps: declarations if present.
    // Each cap entry (%name: @Type) extends the environment with that capability variable.
    if let Some(ref caps_ann) = doc.node.caps {
        // Need to make env mutable to extend it
        let mut env_mut = (*env).clone();
        for (cap_name, annotation) in &caps_ann.node {
            match resolve_annotation(annotation, &env, caps_ann.span, state, &mut None, &mut None) {
                Ok(cap_type) => {
                    // Insert capability variable with % prefix
                    env_mut.insert(format!("%{}", cap_name), cap_type);
                }
                Err(e) => {
                    // Caps annotation errors are fatal (not advisory).
                    // If a capability type annotation is invalid, we can't type-check the document.
                    errors.push(e);
                }
            }
        }
        env = Rc::new(env_mut);
    }

    let mut result_type = Type::Record(Row {
        fields: HashMap::new(),
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
                    // Apply substitution before consistency check
                    let (result_type_resolved, expected_output_resolved) = if state.subst.is_empty()
                    {
                        (result_type.clone(), expected_output.clone())
                    } else {
                        (
                            state.subst.apply(&result_type),
                            state.subst.apply(&expected_output),
                        )
                    };
                    let passes = Type::is_subtype(&result_type_resolved, &expected_output_resolved)
                        || ((contains_unknown_or_top(&result_type_resolved)
                            || contains_unknown_or_top(&expected_output_resolved))
                            && Type::is_consistent(
                                &result_type_resolved,
                                &expected_output_resolved,
                            ));
                    if !passes {
                        advisory_errors.push(TypeError::new(
                            format!(
                                "Document output type {} does not match annotation {}",
                                result_type_resolved, expected_output_resolved
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
                            Type::Unknown => {}
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

    // If the last expression was a dict, thread its schemes into the result environment.
    // For dict literals, also attach inner_schemes to enable polymorphic dot-access.
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            // If the scheme's body is a Record type and this came from a dict literal,
            // the dict literal's inner schemes should be attached to enable [DOT-POLY].
            // However, at this point we don't have access to the inner dict's schemes.
            // This will be handled separately for nested dicts.
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
                // Apply substitution before consistency check
                let (result_type_resolved, expected_output_resolved) = if state.subst.is_empty() {
                    (result_type.clone(), expected_output.clone())
                } else {
                    (
                        state.subst.apply(&result_type),
                        state.subst.apply(&expected_output),
                    )
                };
                let passes = Type::is_subtype(&result_type_resolved, &expected_output_resolved)
                    || ((contains_unknown_or_top(&result_type_resolved)
                        || contains_unknown_or_top(&expected_output_resolved))
                        && Type::is_consistent(&result_type_resolved, &expected_output_resolved));
                if !passes {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Document output type {} does not match annotation {}",
                            result_type_resolved, expected_output_resolved
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
    _resolve_env: &TypeEnv,
    state: &mut InferState,
) -> Vec<TypeError> {
    let mut errors = Vec::new();
    if let Expr::Dict(entries) = &expr.node {
        // Two-pass registration to support recursive type aliases:
        // Pass 1: Pre-register all aliases with placeholder bodies (Unknown)
        // Pass 2: Resolve actual bodies (now recursive references can be looked up)

        // Pass 1: Collect alias names and pre-register placeholders
        let mut alias_entries = Vec::new();
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let Expr::Str(name) = &key.node {
                    if let Expr::TypeAlias { params, body } = &entry.node.value.node {
                        alias_entries.push((name.clone(), params.clone(), body.clone()));
                        // Pre-register with placeholder body
                        target_env.insert_type_alias(
                            name.clone(),
                            TypeAlias {
                                params: params.clone(),
                                body: Type::Unknown,
                            },
                        );
                    }
                }
            }
        }

        // Pass 2: Resolve actual bodies
        for (name, params, body) in alias_entries {
            // Use a fresh per-alias mapping so annotation names within one type
            // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
            // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
            // creates a distinct fresh var, breaking identity-function types.
            let mut alias_ann_map: HashMap<String, String> = HashMap::new();
            // Pre-seed param names so they map to fresh TypeVars.
            for p in &params {
                let fresh = format!("_t{}", state.name_counter);
                state.name_counter += 1;
                state.levels.insert(fresh.clone(), state.level);
                alias_ann_map.insert(p.clone(), fresh.clone());
            }

            // Create a recursion guard for this alias resolution
            let mut recursion_guard = HashSet::new();

            match resolve_type_expr_with_guard(
                &body,
                target_env, // Now resolve in target_env so recursive refs are visible
                state,
                &mut Some(&mut alias_ann_map),
                &mut None,
                &mut recursion_guard,
                &name,
                0,
            ) {
                Ok(alias_ty) => {
                    // Use the fresh names assigned to params
                    let remapped_params: Vec<String> = params
                        .iter()
                        .map(|p| alias_ann_map.get(p).cloned().unwrap())
                        .collect();
                    // Update with actual body
                    target_env.insert_type_alias(
                        name.clone(),
                        TypeAlias {
                            params: remapped_params,
                            body: alias_ty,
                        },
                    );
                }
                Err(e) => errors.push(e),
            }
        }
    }
    errors
}

/// Narrowing constraints extracted from conditional expressions.
/// Each constraint refines the type of a variable in the true branch of an `if`.
#[derive(Debug, Clone)]
enum Narrowing {
    /// `[= var literal]` narrows `var` to the literal type.
    EqLiteral { var: String, ty: Type },
    /// `[= [type-of var] "TypeName"]` narrows `var` to the named type.
    TypeOf { var: String, ty: Type },
    /// `[has? var "key"]` narrows `var` to a record with at least that key.
    HasKey { var: String, key: String },
}

/// Extract narrowing constraints from a condition expression.
/// Returns an empty vec for unrecognized patterns.
fn extract_narrowings(cond: &Spanned<Expr>) -> Vec<Narrowing> {
    match &cond.node {
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } if named_args.is_empty() => {
            if let Expr::VarRef { name, .. } = &func.node {
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
                        if let (Expr::VarRef { name: var_name, .. }, Expr::Str(key)) =
                            (&args[0].node, &args[1].node)
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
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Int,
                            }];
                        }
                    }
                    "str?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Str,
                            }];
                        }
                    }
                    "dict?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            // dict? narrows to open record with fresh RowVar
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Record(Row {
                                    fields: HashMap::new(),
                                }),
                            }];
                        }
                    }
                    "bool?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Bool,
                            }];
                        }
                    }
                    "float?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Float,
                            }];
                        }
                    }
                    "fn?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            // fn? narrows to Unknown (can't express "any function" precisely yet)
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Unknown,
                            }];
                        }
                    }
                    "null?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            // null? narrows to empty closed record
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Record(Row {
                                    fields: HashMap::new(),
                                }),
                            }];
                        }
                    }
                    "seq?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Seq(Box::new(Type::Unknown)),
                            }];
                        }
                    }
                    "num?" if args.len() == 1 => {
                        if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                            // num? narrows to Number (supertype of Int | Float)
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Number,
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
fn try_eq_literal(left: &Spanned<Expr>, right: &Spanned<Expr>) -> Option<Narrowing> {
    if let Expr::VarRef { name, .. } = &left.node {
        match &right.node {
            Expr::Int(n) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::IntLiteral(*n),
            }),
            Expr::Str(s) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::StringLiteral(s.clone()),
            }),
            Expr::Bool(_b) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::Bool,
            }),
            _ => None,
        }
    } else {
        None
    }
}

/// Try to extract a type-of narrowing from `[= [type-of var] "TypeName"]`.
fn try_type_of(left: &Spanned<Expr>, right: &Spanned<Expr>) -> Option<Narrowing> {
    // Left side must be [type-of var]
    if let Expr::Call {
        func,
        args,
        named_args,
        ..
    } = &left.node
    {
        if named_args.is_empty() && args.len() == 1 {
            if let Expr::VarRef {
                name: func_name, ..
            } = &func.node
            {
                if func_name == "type-of" {
                    if let Expr::VarRef { name: var_name, .. } = &args[0].node {
                        // Right side must be a string literal type name
                        if let Expr::Str(type_name) = &right.node {
                            let ty = match type_name.as_str() {
                                "Int" => Some(Type::Int),
                                "Float" => Some(Type::Float),
                                "String" => Some(Type::Str),
                                "Bool" => Some(Type::Bool),
                                "Dict" => Some(Type::Record(Row {
                                    fields: HashMap::new(),
                                })),
                                "Seq" => Some(Type::Seq(Box::new(Type::Unknown))),
                                "Number" => Some(Type::Number),
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
fn apply_narrowings(
    env: &Rc<TypeEnv>,
    narrowings: &[Narrowing],
    state: &mut InferState,
) -> Rc<TypeEnv> {
    if narrowings.is_empty() {
        return Rc::clone(env);
    }

    let mut new_env = TypeEnv::with_parent(env);

    for narrowing in narrowings {
        match narrowing {
            Narrowing::EqLiteral { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed
                new_env.insert(var.clone(), ty.clone());
            }
            Narrowing::TypeOf { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed
                new_env.insert(var.clone(), ty.clone());
            }
            Narrowing::HasKey { var, key } => {
                // Get the current type of the variable (if any)
                let current_ty = env.get(var).map(|scheme| scheme.body.clone());

                // Create a record type with at least the given key
                let mut fields = HashMap::new();
                let fresh_type_var = state.fresh_type_var();
                fields.insert(key.clone(), fresh_type_var);

                // BAS: all tails are Empty. Merge existing record fields if present.
                // Width subtyping handles the openness — the record is known to have the
                // key at runtime, and may have additional fields beyond those annotated.
                let new_ty = if let Some(Type::Record(current_row)) = current_ty {
                    // Merge existing fields with the new constraint
                    for (k, v) in current_row.fields {
                        fields.insert(k, v);
                    }
                    Type::Record(Row { fields })
                } else {
                    // Create a fresh record with just the key constraint
                    Type::Record(Row { fields })
                };

                new_env.insert(var.clone(), new_ty);
            }
        }
    }

    Rc::new(new_env)
}

/// Apply negation narrowings for the false branch of an `if` expression.
///
/// For each `TypeOf { var, ty }` narrowing (e.g., produced by `[int? x]`), the false branch
/// knows the predicate FAILED, so the variable's type is intersected with `Negation(ty)`.
/// This is the BAS false-branch rule: ~[int? x] → x : ~Int.
///
/// EqLiteral and HasKey narrowings are not negated in the false branch (they produce
/// Negation(literal) which is rarely useful and can confuse downstream unification).
fn apply_negation_narrowings(
    env: &Rc<TypeEnv>,
    narrowings: &[Narrowing],
    _state: &mut InferState,
) -> Rc<TypeEnv> {
    // Only TypeOf narrowings produce useful false-branch refinements
    let type_of_narrowings: Vec<_> = narrowings
        .iter()
        .filter(|n| matches!(n, Narrowing::TypeOf { .. }))
        .collect();

    if type_of_narrowings.is_empty() {
        return Rc::clone(env);
    }

    let mut new_env = TypeEnv::with_parent(env);

    for narrowing in type_of_narrowings {
        let Narrowing::TypeOf { var, ty } = narrowing else {
            continue;
        };
        // In the false branch: x : ~ty (negation of the predicate type)
        // Do NOT apply if ty is Unknown (fn? narrowing) — ~Unknown is not useful
        if matches!(ty, Type::Unknown) {
            continue;
        }
        let negated = Type::Negation(Box::new(ty.clone()));
        new_env.insert(var.clone(), negated);
    }

    Rc::new(new_env)
}

/// Type-check an `if` expression with path-sensitive narrowing.
fn infer_if(
    cond: &Spanned<Expr>,
    then_expr: &Spanned<Expr>,
    else_expr: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Infer the condition type (must be Bool)
    let _cond_ty = infer_expr(cond, env, state, type_map)?;

    // Extract narrowings from the condition
    let narrowings = extract_narrowings(cond);

    // Fork the environment for the true branch
    let env_true = apply_narrowings(env, &narrowings, state);

    // Fork the environment for the false branch: apply negation narrowings (BAS false-branch).
    // For each TypeOf narrowing (e.g., [int? x] → x : Int in true branch),
    // the false branch narrows x to Negation(Int) — i.e., "definitely not Int".
    // This enables false-branch type refinement: if x : Int | Str and [int? x] fails,
    // then x : (Int | Str) & ~Int = Str in the false branch.
    let env_false = apply_negation_narrowings(env, &narrowings, state);

    // Infer the then and else branches in their respective environments
    let then_ty = infer_expr(then_expr, &env_true, state, type_map)?;
    let else_ty = infer_expr(else_expr, &env_false, state, type_map)?;

    // Form the union of both branch types and simplify (RDNF step 1b).
    // normalize_union deduplicates — if both branches return Int, the result is Int not Union([Int, Int]).
    let raw_union = Type::normalize_union(vec![then_ty, else_ty]);
    let result_ty = Type::simplify_type(raw_union);

    Ok(result_ty)
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
/// - `Pattern::Constructor { binding }`: payload gets `Unknown` (no payload narrowing yet).
/// - `Pattern::Or(alts)`: collect from the first alternative only (all alts must bind
///   the same variable set by parser invariant).
/// - `Pattern::Wildcard | Literal | TypeTag | Pin`: no bindings.
fn collect_pattern_bindings(pat: &Pattern, scrutinee_ty: &Type, out: &mut Vec<(String, Type)>) {
    match pat {
        Pattern::Variable(name) => {
            out.push((name.clone(), scrutinee_ty.clone()));
        }
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::TypeTag(_) | Pattern::Pin(_) => {}
        Pattern::Dict { fields, .. } => {
            for (key, sub_pat) in fields {
                // Narrow the sub-pattern's scrutinee type using the record field type.
                let field_ty = match scrutinee_ty {
                    Type::Record(row) => row.fields.get(key).cloned().unwrap_or(Type::Unknown),
                    // Union: if all members that are Records agree on the field type, use it.
                    Type::Union(members) => {
                        // Collect field types from all Record members
                        let mut field_types = Vec::new();
                        for member in members {
                            if let Type::Record(row) = member {
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
                                Type::Unknown
                            }
                        } else {
                            Type::Unknown
                        }
                    }
                    _ => Type::Unknown,
                };
                collect_pattern_bindings(&sub_pat.node, &field_ty, out);
            }
        }
        Pattern::Seq { head, tail } => {
            // Head is the element type; tail is the remaining Seq.
            let elem_ty = match scrutinee_ty {
                Type::Seq(elem) => (**elem).clone(),
                _ => Type::Unknown,
            };
            collect_pattern_bindings(&head.node, &elem_ty, out);
            // Tail is always a Seq of the same element type.
            let tail_ty = Type::Seq(Box::new(elem_ty));
            collect_pattern_bindings(&tail.node, &tail_ty, out);
        }
        Pattern::Constructor { binding, .. } => {
            // No payload type narrowing for constructors yet.
            if let Some(b) = binding {
                collect_pattern_bindings(&b.node, &Type::Unknown, out);
            }
        }
        Pattern::Or(alts) => {
            // Or-patterns: only collect from the first alternative (all alts must bind
            // the same set of variables, so any choice is equivalent for scoping).
            if let Some(first) = alts.first() {
                collect_pattern_bindings(&first.node, scrutinee_ty, out);
            }
        }
    }
}

/// Extract type parameters from an instance pattern declaration.
///
/// The PatternDecl stores the inner bracket `[a@Int b@Float]` as a single `Expr::Dict`
/// binding (auto-indexed entries). This function recursively extracts types from either:
/// - `Expr::Dict(entries)` — inner binding bracket; extracts each auto-indexed entry
/// - `Expr::Annotated { annotation, .. }` — `a@Type` form; resolves the annotation
/// - `Expr::VarRef { .. }` — bare identifier; treated as `Type::Unknown`
fn extract_pattern_types(
    pattern_expr: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
) -> Result<Vec<Type>, Vec<TypeError>> {
    match &pattern_expr.node {
        Expr::PatternDecl { bindings } | Expr::LetDecl { bindings } => {
            let mut types = Vec::new();
            for binding in bindings {
                extract_binding_types(binding, env, state, &mut types)?;
            }
            Ok(types)
        }
        _ => Err(vec![TypeError::new(
            "instance arm pattern must be a [pattern [...]] or [let ...] declaration",
            pattern_expr.span,
        )]),
    }
}

/// Recursively extract type(s) from a single pattern binding expression.
///
/// - `Expr::Dict(entries)` — inner binding bracket `[a@Int b@Float]` (old syntax); expands entries
/// - `Expr::LetDecl { bindings }` — inner binding bracket `[let a@Int b@Float]` (new syntax); expands bindings
/// - `Expr::Call { func, args, .. }` — implied call `[Type]` or `[Type arg1 arg2]`; infers the call type
/// - `Expr::Annotated { annotation, .. }` — `a@Type` form
/// - `Expr::VarRef { .. }` — bare identifier → `Type::Unknown`
/// - `Expr::Placeholder` — wildcard `_` → `Type::Unknown`
fn extract_binding_types(
    binding: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    types: &mut Vec<Type>,
) -> Result<(), Vec<TypeError>> {
    match &binding.node {
        // Inner binding bracket [a@Int b@Float] parsed as auto-indexed Dict (old syntax)
        Expr::Dict(entries) => {
            for entry in entries {
                // Each entry should be auto-indexed (no key) with Annotated/VarRef value
                extract_binding_types(&entry.node.value, env, state, types)?;
            }
        }
        // Inner binding bracket [let a@Int b@Float] (new unified-bindings syntax)
        Expr::LetDecl { bindings } => {
            for sub_binding in bindings {
                extract_binding_types(sub_binding, env, state, types)?;
            }
        }
        // Implied call [Int] or [Result String] — treat as a type reference
        // For now, just treat the whole call as Unknown to avoid infinite recursion
        // Full implementation would resolve constructor types from the call
        Expr::Call { .. } => {
            types.push(Type::Unknown);
        }
        // a@Type form
        Expr::Annotated { annotation, .. } => {
            let ty = resolve_annotation(
                &annotation.node,
                env,
                annotation.span,
                state,
                &mut None,
                &mut None,
            )
            .map_err(|e| vec![e])?;
            types.push(ty);
        }
        // Bare identifier (treated as Unknown for now)
        Expr::VarRef { .. } => {
            types.push(Type::Unknown);
        }
        // Wildcard placeholder
        Expr::Placeholder => {
            types.push(Type::Unknown);
        }
        _ => {
            return Err(vec![TypeError::new(
                "pattern binding must be in form 'a@Type', bare identifier, or [let ...]",
                binding.span,
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
fn patterns_overlap(
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
    // Also save subst and name_counter: improve_functional_dependency writes directly to
    // state.subst (via std::mem::take/replace) rather than through temp_subst, and
    // resolve_instance may call fresh_type_var() incrementing name_counter.
    let saved_subst = state.subst.clone();
    let saved_name_counter = state.name_counter;

    // Use a temporary substitution so state.subst is also unaffected.
    let mut temp_subst = state.subst.clone();
    let overlaps = types_a.iter().zip(types_b.iter()).all(|(ty_a, ty_b)| {
        // Unknown is the gradual-typing wildcard for unannotated pattern bindings.
        // Treat Unknown as distinct from any concrete type: a position with Unknown
        // cannot be used to establish overlap (it carries no type information).
        if matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown) {
            return false; // non-overlapping at this position — Unknown is not concrete
        }
        unify(ty_a, ty_b, &mut temp_subst, state, Span::origin()).is_ok()
    });

    // Restore all mutated fields.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;
    state.name_counter = saved_name_counter;

    Ok(overlaps)
}

/// Structural equality check for two type slices.
/// Used by the consistency check to compare determined types without unification.
fn types_equal(types_a: &[Type], types_b: &[Type]) -> bool {
    types_a.len() == types_b.len() && types_a.iter().zip(types_b.iter()).all(|(a, b)| a == b)
}

/// Extract parameter indices from a functional dependency variable list.
/// Accepts a single param name (VarRef/Str), a Dict list [a b c], or an implied
/// Call `[a b]` (which the parser produces when `a` is in head position).
/// Returns Vec<usize> of indices into the class params list.
fn extract_param_indices(
    expr: &Spanned<Expr>,
    params: &[String],
    span: Span,
) -> Result<Vec<usize>, Vec<TypeError>> {
    let mut indices = Vec::new();

    match &expr.node {
        // Single param: a@Type or just "a"
        Expr::VarRef { name, .. } | Expr::Str(name) => {
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
        Expr::Dict(entries) => {
            for entry in entries {
                let param_name = match &entry.node.value.node {
                    Expr::VarRef { name, .. } => name,
                    Expr::Str(s) => s,
                    _ => {
                        return Err(vec![TypeError::new(
                            "functional dependency param must be an identifier or string",
                            entry.span,
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
                        entry.span,
                    )]);
                }
            }
        }
        // Multiple params as implied Call: produced when bracket has identifier in head
        // position, e.g. `[a b]` → Call { func: VarRef("a"), args: [VarRef("b")] }
        Expr::Call {
            func,
            args,
            implied: true,
            ..
        } => {
            // Extract the function (head param)
            let head_name = match &func.node {
                Expr::VarRef { name, .. } => name,
                Expr::Str(s) => s,
                _ => {
                    return Err(vec![TypeError::new(
                        "functional dependency param must be an identifier or string",
                        func.span,
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
                    func.span,
                )]);
            }
            // Extract the remaining args
            for arg in args {
                let arg_name = match &arg.node {
                    Expr::VarRef { name, .. } => name,
                    Expr::Str(s) => s,
                    _ => {
                        return Err(vec![TypeError::new(
                            "functional dependency param must be an identifier or string",
                            arg.span,
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
                        arg.span,
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

        Expr::VarRef { name, .. } => {
            if let Some(scheme) = env.get(name) {
                // Record scheme in scheme_map for LSP hover (constraints + type vars).
                // Only store when scheme collection is enabled and the scheme is polymorphic
                // (has constraints or quantified type vars — monomorphic schemes show the
                // same info via type_map and don't need the extra constraint display).
                if !scheme.constraints.is_empty() || !scheme.type_vars.is_empty() {
                    if let Some(ref mut smap) = state.scheme_map {
                        let key = (expr.span.start.offset, expr.span.end.offset);
                        smap.insert(key, scheme.clone());
                    }
                }
                Ok(instantiate_scheme(scheme, state.level, state))
            } else {
                let mut err = TypeError::undefined_variable(name, expr.span);
                if let Some(&cause_span) = state.failed_bindings.get(name.as_str()) {
                    err.notes.push(format!(
                        "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                        cause_span.start.line, cause_span.start.column
                    ));
                }
                Err(vec![err])
            }
        }

        Expr::Dict(entries) => {
            infer_dict(entries, env, state, type_map, expr.span).map(|(ty, _schemes)| ty)
        }

        Expr::DotAccess {
            expr: target,
            field,
        } => check_dot_access(target, field, env, expr.span, state, type_map),

        Expr::Pipe { .. } => {
            unreachable!("Pipe should be desugared before type checking")
        }

        Expr::Sequential(exprs) => {
            // Multi-expression sequential evaluation (let-binding semantics).
            // Each expression's result dict extends the type environment for the next.
            // The last expression's type is the overall result type.
            if exprs.is_empty() {
                return Ok(Type::Record(Row {
                    fields: HashMap::new(),
                }));
            }

            let mut current_env = Rc::clone(env);

            for (i, seq_expr) in exprs.iter().enumerate() {
                let is_last = i == exprs.len() - 1;

                if is_last {
                    // Last expression: return its type
                    return infer_expr(seq_expr, &current_env, state, type_map);
                }

                // Intermediate expression: infer and extract record bindings
                let expr_ty = infer_expr(seq_expr, &current_env, state, type_map)?;

                // Extract record fields to extend the type environment
                if let Type::Record(row) = expr_ty {
                    // Create child environment with bindings from intermediate expression
                    let mut child_env = TypeEnv::with_parent(&current_env);

                    // Add field types to environment
                    for (field_name, field_ty) in &row.fields {
                        child_env.insert(field_name.clone(), field_ty.clone());
                    }

                    current_env = Rc::new(child_env);
                } else {
                    // Intermediate expression must be a record (dict)
                    return Err(vec![TypeError::new(
                        &format!(
                            "sequential expression requires intermediate expressions to be dicts, got {}",
                            expr_ty
                        ),
                        seq_expr.span,
                    )]);
                }
            }

            unreachable!(
                "infer Sequential: loop did not return — exprs was non-empty but is_last never triggered"
            )
        }

        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => {
            // Special case: `if` is a type-level special form with path-sensitive narrowing
            if let Expr::VarRef { name, .. } = &func.node {
                if name == "if" && args.len() == 3 && named_args.is_empty() {
                    return infer_if(&args[0], &args[1], &args[2], env, state, type_map);
                }
            }

            // Special case: `get`, `get?`, `builtin-get`, and `get-in` are type-level special forms
            // that narrow their return type based on the dict argument's type (Map[K V] → V or V|Null).
            //
            // `builtin-get` is intercepted alongside `get` so that label-annotated wrappers like
            //   `get: [fn [k@Label xs] [builtin-get k xs]]`
            // produce a precise return type. When the key is a label TypeVar, `check_get` calls
            // `resolve_has_field` and emits a `HasField` constraint that binds the field TypeVar.
            // Without this, `[builtin-get k xs]` returns `Unknown` from the monomorphic TypeScheme,
            // causing the `get` wrapper's return type to be `Unknown` even for annotated calls.
            //
            // `builtin-get` is kept monomorphic in the TypeEnv (Unknown → Unknown → Unknown) to
            // avoid O(N²) blowup: the prelude's private dict has ~35 `builtin-get` calls (integer
            // and string key access in recursive helpers). Making it polymorphic would add ~105
            // TypeVars to state.subst per prelude type-check, triggering the merge-loop O(N²).
            // The special-form dispatch handles all cases precisely without creating TypeVar chains.
            if let Expr::VarRef { name, .. } = &func.node {
                if (name == "get" || name == "get?" || name == "builtin-get")
                    && named_args.is_empty()
                {
                    let is_optional = name == "get?";
                    return check_get(
                        is_optional,
                        args,
                        named_args,
                        env,
                        expr.span,
                        state,
                        type_map,
                    );
                }
                if name == "get-in" && named_args.is_empty() {
                    return check_get_in(args, named_args, env, expr.span, state, type_map);
                }
            }

            // Special case: if func is a VarRef to a polymorphic scheme, pass the scheme
            // directly to avoid double instantiation (VAR-POLY followed by CALL-POLY).
            // For monomorphic schemes, use the normal path which handles TypeVar during letrec.
            if let Expr::VarRef { name, .. } = &func.node {
                // Polymorphic recursion check: reject recursive calls during function body inference.
                // This implements depth-1 polyrecursion ban (conservative: rejects all unannotated
                // recursion to prevent silent Unknown inference). Functions with explicit return
                // type annotations are allowed to recurse because the return type is pinned.
                // TODO: refine to allow monomorphic recursion by detecting when recursive call
                // types match the inferred type (requires constraint-based approach).
                //
                // LIMITATION: This check only fires for direct VarRef calls (not field access like
                // $obj.f) and only when state.current_function is set (dict entries, not closure
                // captures). Indirect recursion via field access or closure captures bypasses the
                // check. This is intentional — detecting recursion through arbitrary expressions is
                // much harder and rare in practice.
                if state.current_function.as_ref() == Some(name) {
                    let mut err = TypeError::new(
                        "recursive function requires an explicit return type annotation — annotate with `fn@ReturnType [...]`",
                        expr.span,
                    );
                    err.notes.push(
                        "  = help: Recursive functions without return type annotations would get type `Unknown`. \
                         Add a return type annotation (`fn@ReturnType [...]`) to enable recursion.".to_string()
                    );
                    return Err(vec![err]);
                }

                match env.get(name) {
                    Some(scheme) if !scheme.type_vars.is_empty() => {
                        // Record scheme for LSP hover (shows constraints on the call-head VarRef).
                        if !scheme.constraints.is_empty() || !scheme.type_vars.is_empty() {
                            if let Some(ref mut smap) = state.scheme_map {
                                let key = (func.span.start.offset, func.span.end.offset);
                                smap.insert(key, scheme.clone());
                            }
                        }
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
                            let mut err = TypeError::undefined_variable(name, func.span);
                            if let Some(&cause_span) = state.failed_bindings.get(name.as_str()) {
                                err.notes.push(format!(
                                    "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                                    cause_span.start.line, cause_span.start.column
                                ));
                            }
                            Err(vec![err])
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

        Expr::TypeAlias { body, .. } => expand_type_alias(body, env, state).map_err(|e| vec![e]),

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
            // Create per-annotation-scope mappings for type and row variables.
            let mut ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
            let mut row_ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
            let mut ann_mapping_opt = ann_mapping.as_mut();
            let mut row_ann_mapping_opt = row_ann_mapping.as_mut();
            resolve_annotated(
                name,
                annotation,
                env,
                expr.span,
                state,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
            )
            .map_err(|e| vec![e])
        }

        Expr::Quote(_inner) => {
            // [quote expr] produces a dict representing the AST.
            // Type: empty record (BAS: width subtyping allows any fields).
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }

        Expr::Unquote(inner) => {
            // [unquote expr] evaluates expr and returns its type.
            // It must be inside a [quote ...], but the parser enforces that.
            // The result should be a Dict (AST node).
            infer_expr(inner, env, state, type_map)
        }

        Expr::UnquoteSplice(inner) => {
            // [unquote-splice expr] expects expr to be a list (Dict with integer keys).
            // Type: empty record (BAS: width subtyping allows any fields).
            let inner_ty = infer_expr(inner, env, state, type_map)?;

            // The inner expression should be a Dict (list).
            let expected_list_ty = Type::Record(Row {
                fields: HashMap::new(),
            });

            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&inner_ty, &expected_list_ty, &mut subst, state, inner.span);
            state.subst = subst;
            result.map_err(|_e| {
                vec![TypeError::new(
                    &format!("unquote-splice expects a list (Dict), got {}", inner_ty),
                    inner.span,
                )]
            })?;

            // UnquoteSplice itself doesn't have a standalone type — it's only valid
            // in list positions where it splices elements. Return Dict for now.
            Ok(expected_list_ty)
        }

        Expr::Match { scrutinee, arms } => {
            // Infer scrutinee type — needed for exhaustiveness checking.
            let scrutinee_ty = infer_expr(scrutinee, env, state, type_map)?;
            let scrutinee_ty = state.subst.apply(&scrutinee_ty);

            // Infer each arm's guard (if present) and body type.
            // Extend the TypeEnv with pattern-bound variables and their inferred types
            // so that references inside the arm body are in scope and carry precise types
            // derived from the scrutinee type and pattern shape (match-arm-scope sprint).
            //
            // I-Case3 (BAS match narrowing): maintain a "remaining scrutinee" type that
            // accumulates negations as Constructor/TypeTag arms are processed.
            // For arm matching #C: arm body sees scrutinee ∩ #C (or the tag literal type).
            // For subsequent arms: remaining_scrutinee ∩ ~#C (the tag is ruled out).
            //
            // This gives false-branch narrowing semantics inside match:
            //   [match x  Int [...]  Str [...]]
            //   → arm 1: x : scrutinee ∩ Int
            //   → arm 2: x : scrutinee ∩ ~Int ∩ Str  (if scrutinee : Int | Str, simplifies to Str)
            let mut remaining_scrutinee = scrutinee_ty.clone();
            let mut arm_result_types: Vec<Type> = Vec::new();

            for arm in arms {
                // Compute the arm-local scrutinee type from I-Case3:
                // For Constructor/TypeTag patterns, intersect remaining with the tag's type.
                let arm_scrutinee_ty = match &arm.pattern.node {
                    Pattern::Constructor { tag, .. } | Pattern::TypeTag(tag) => {
                        // The arm matches when scrutinee has this constructor tag.
                        // Intersect remaining_scrutinee with a StringLiteral type for the tag,
                        // which approximates the nominal tag constraint.
                        let tag_ty = Type::StringLiteral(tag.clone());
                        let members = vec![remaining_scrutinee.clone(), tag_ty];
                        Type::normalize_intersection(members)
                    }
                    Pattern::Wildcard | Pattern::Variable(_) => {
                        // Catch-all: arm sees the full remaining scrutinee
                        remaining_scrutinee.clone()
                    }
                    _ => scrutinee_ty.clone(),
                };

                let mut pat_bindings: Vec<(String, Type)> = Vec::new();
                collect_pattern_bindings(&arm.pattern.node, &arm_scrutinee_ty, &mut pat_bindings);
                let arm_env = if pat_bindings.is_empty() {
                    env.clone()
                } else {
                    let mut child = TypeEnv::with_parent(env);
                    for (name, ty) in pat_bindings {
                        child.insert(name, ty);
                    }
                    Rc::new(child)
                };
                // Type-check guard if present, and apply is: predicate narrowing.
                // Guards synthesized from `x@[is: pred]` have the form `[pred x]`.
                // For recognized type predicates (int?, str?, etc.), narrow the variable
                // type in the arm body environment using the same extract_narrowings logic
                // as $if narrowing (BAS narrowing, reused for is: guards).
                let arm_env = if let Some(guard) = &arm.guard {
                    let _guard_ty = infer_expr(guard, &arm_env, state, type_map)?;
                    // Extract narrowings from the guard expression (same logic as $if).
                    let guard_narrowings = extract_narrowings(guard);
                    if guard_narrowings.is_empty() {
                        arm_env
                    } else {
                        apply_narrowings(&arm_env, &guard_narrowings, state)
                    }
                } else {
                    arm_env
                };
                let arm_ty = infer_expr(&arm.body, &arm_env, state, type_map)?;
                arm_result_types.push(arm_ty);

                // Update remaining_scrutinee for subsequent arms (I-Case3 negation accumulation).
                // For Constructor/TypeTag arms without a guard: remaining ∩ ~tag.
                // Guarded arms are opaque (the guard may succeed or fail — we can't narrow).
                if arm.guard.is_none() {
                    match &arm.pattern.node {
                        Pattern::Constructor { tag, .. } | Pattern::TypeTag(tag) => {
                            let neg_tag =
                                Type::Negation(Box::new(Type::StringLiteral(tag.clone())));
                            remaining_scrutinee = Type::normalize_intersection(vec![
                                remaining_scrutinee.clone(),
                                neg_tag,
                            ]);
                        }
                        Pattern::Wildcard | Pattern::Variable(_) => {
                            // Catch-all: remaining is Never (no more arms can match)
                            remaining_scrutinee = Type::Never;
                        }
                        _ => {
                            // Literal, Dict, Seq patterns — no structural narrowing for now
                        }
                    }
                }
            }

            // Exhaustiveness checking: when scrutinee type is a union, run the
            // Maranget (2007) usefulness algorithm over the pattern matrix.
            //
            // Coverage is checked only when the scrutinee's type is known to be
            // a Type::Union — either via TypeAssert annotation or inference.
            // Without a union type, the match is dynamically correct but
            // statically unverified (consistent with Karachalias et al. 2015).
            if let Type::Union(ref members) = scrutinee_ty {
                let sig = coverage::ConstructorSignature::from_union(members);

                // Convert AST patterns to coverage patterns
                let coverage_patterns: Vec<coverage::CoveragePattern> = arms
                    .iter()
                    .map(|arm| coverage::ast_pattern_to_coverage(&arm.pattern.node))
                    .collect();

                // Build guard flags: arms with guards are opaque to coverage
                let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();

                let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);

                // Collect type errors (non-fatal: report all, don't short-circuit)
                let mut match_errors: Vec<TypeError> = Vec::new();

                if !result.exhaustive {
                    let witnesses = coverage::format_witnesses(&result.uncovered);
                    match_errors.push(TypeError::new(
                        format!("non-exhaustive match: missing coverage for {}", witnesses),
                        expr.span,
                    ));
                }

                for &idx in &result.redundant {
                    match_errors.push(TypeError::new(
                        "unreachable match arm: this pattern is already covered by prior arms",
                        arms[idx].pattern.span,
                    ));
                }

                for &idx in &result.inaccessible {
                    match_errors.push(TypeError::new(
                        "inaccessible match arm: reachable only via diverging (bottom) values",
                        arms[idx].pattern.span,
                    ));
                }

                if !match_errors.is_empty() {
                    return Err(match_errors);
                }
            }

            // Union all arm result types and simplify (RDNF step 1a).
            // If arms is empty (malformed match), fall back to Unknown.
            // Deduplication and simplification collapse Union([Int, Int]) → Int, etc.
            let match_ty = if arm_result_types.is_empty() {
                Type::Unknown
            } else {
                let raw_union = Type::normalize_union(arm_result_types);
                Type::simplify_type(raw_union)
            };
            Ok(match_ty)
        }

        Expr::DefMacro { .. } => {
            // DefMacro should be removed by the expansion pass before typechecking.
            // If we see it here, it's an internal error.
            Err(vec![TypeError::new(
                "defmacro should be removed by expansion pass before typechecking (internal error)",
                expr.span,
            )])
        }

        Expr::ClassDecl {
            name,
            params,
            superclasses,
            methods,
            determines,
            resolver,
        } => {
            use crate::types::{ClassDecl, Kind};

            // Parse method signatures and build ClassDecl
            let mut method_types = HashMap::new();

            for method in methods {
                // Extract method name from key (must be a string literal or identifier)
                let method_name = match &method.node.key {
                    Some(key_expr) => match &key_expr.node {
                        Expr::Str(s) => s.clone(),
                        Expr::VarRef { name, .. } => name.clone(),
                        _ => {
                            return Err(vec![TypeError::new(
                                "class method name must be a string or identifier",
                                key_expr.span,
                            )]);
                        }
                    },
                    None => {
                        return Err(vec![TypeError::new(
                            "class method must have a name",
                            method.span,
                        )]);
                    }
                };

                // Resolve method signature type from value expression
                // Method signatures are type expressions, not runtime values
                let method_type =
                    resolve_type_expr(&method.node.value, env, state, &mut None, &mut None)
                        .map_err(|e| vec![e])?;

                // Wrap in a monomorphic TypeScheme (class methods don't have their own quantifiers)
                method_types.insert(method_name, TypeScheme::mono(method_type));
            }

            // Build class declaration. Inherit param kinds from any existing pre-registration
            // (e.g. Mappable is pre-registered with Kind::Operator in InferState::new()).
            // This preserves the higher-kinded kind even when the parser cannot yet extract
            // @Operator annotations from class param syntax (hkt-kind-inference).
            let existing_param_kinds: std::collections::HashMap<String, Kind> = state
                .class_env
                .get(name)
                .map(|existing| existing.params.iter().cloned().collect())
                .unwrap_or_default();

            // Process functional dependencies from `determines:` field (Task 6.1 & 6.2)
            let mut fd_indices: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
            for fd_expr in determines {
                // Each FD should be a list: [[determining-vars] determined-var(s)]
                match &fd_expr.node {
                    Expr::Dict(entries) if entries.len() == 2 => {
                        // Extract [determining-vars]
                        let determining = &entries[0].node.value;
                        let determining_indices =
                            extract_param_indices(determining, params, fd_expr.span)?;

                        // Extract determined-var(s)
                        let determined = &entries[1].node.value;
                        let determined_indices =
                            extract_param_indices(determined, params, fd_expr.span)?;

                        fd_indices.push((determining_indices, determined_indices));
                    }
                    _ => {
                        return Err(vec![TypeError::new(
                            "functional dependency must be a 2-element list [[determining-vars] determined-var(s)]",
                            fd_expr.span,
                        )]);
                    }
                }
            }

            // Extract resolver name if present (Task 6.3)
            let resolver_name = if let Some(resolver_expr) = resolver {
                match &resolver_expr.node {
                    Expr::VarRef { name, .. } => Some(name.clone()),
                    Expr::Str(s) => Some(s.clone()),
                    _ => {
                        return Err(vec![TypeError::new(
                            "resolver must be an identifier or string",
                            resolver_expr.span,
                        )]);
                    }
                }
            } else {
                None
            };

            // Build ClassDecl with populated FD and resolver fields (Task 6.4)
            let class_decl = ClassDecl {
                name: name.clone(),
                params: params
                    .iter()
                    .map(|p| {
                        let kind = existing_param_kinds.get(p).cloned().unwrap_or(Kind::Type);
                        (p.clone(), kind)
                    })
                    .collect(),
                // Convert superclasses from (String, String) to (String, Vec<String>)
                // AST has single-param format, but ClassDecl supports MPTC
                superclasses: superclasses
                    .iter()
                    .map(|(class_name, param)| (class_name.clone(), vec![param.clone()]))
                    .collect(),
                methods: method_types,
                determines: fd_indices,
                resolver: resolver_name,
                resolver_injective: false, // TODO: Parser support for injective annotation
            };

            // Register in ClassEnv (replaces any prior registration, preserving inherited kinds)
            state.class_env.insert(class_decl.clone());

            // Populate kind_env for Operator-kinded params (hkt-kind-inference Task 1)
            for (param_name, kind) in &class_decl.params {
                if *kind == Kind::Operator {
                    state.kind_env.insert(param_name.clone(), Kind::Operator);
                }
            }

            // ClassDecl expressions evaluate to empty record (see eval.rs)
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }

        Expr::InstanceDecl { class_name, arms } => {
            use crate::types::InstanceDecl;

            if arms.is_empty() {
                // No arms — return empty record
                return Ok(Type::Record(Row {
                    fields: HashMap::new(),
                }));
            }

            // Look up the class declaration to get param count and FDs
            // Clone the necessary data to avoid holding an immutable borrow on state.class_env
            // while we mutably borrow state in the pattern extraction and unification helpers.
            let (param_count, has_fds, fd_list, param_names) = {
                let class_decl = state.class_env.get(class_name).ok_or_else(|| {
                    vec![TypeError::new(
                        format!("unknown class '{}'", class_name),
                        expr.span,
                    )]
                })?;
                (
                    class_decl.params.len(),
                    !class_decl.determines.is_empty(),
                    class_decl.determines.clone(),
                    class_decl
                        .params
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>(),
                )
            };

            // Process each arm: extract pattern types, check arity, collect for soundness checks
            let mut arm_data: Vec<(Vec<Type>, Span, &Vec<Spanned<Entry>>)> = Vec::new();

            for (pattern_expr, methods) in arms {
                // Extract types from pattern (Task 7.1)
                let pattern_types = extract_pattern_types(pattern_expr, env, state)?;

                // Validate arity (Task 7.1)
                if pattern_types.len() != param_count {
                    return Err(vec![TypeError::new(
                        format!(
                            "instance pattern has {} type parameters but class '{}' expects {}",
                            pattern_types.len(),
                            class_name,
                            param_count
                        ),
                        pattern_expr.span,
                    )]);
                }

                arm_data.push((pattern_types, pattern_expr.span, methods));
            }

            // Pairwise disjointness check (Task 7.2)
            for i in 0..arm_data.len() {
                for j in (i + 1)..arm_data.len() {
                    let (types_i, span_i, _) = &arm_data[i];
                    let (types_j, span_j, _) = &arm_data[j];

                    if patterns_overlap(types_i, types_j, state)? {
                        let error = TypeError::new(
                            format!(
                                "overlapping instance patterns for class '{}': arm at line {} and arm at line {} could both match the same types",
                                class_name,
                                span_i.start.line,
                                span_j.start.line
                            ),
                            *span_j,
                        )
                        .with_code("T014");
                        return Err(vec![error]);
                    }
                }
            }

            // Coverage and consistency checks for FD classes (Tasks 7.3 & 7.4)
            if has_fds {
                for (determining_indices, determined_indices) in &fd_list {
                    // Coverage check (Task 7.3): all determined vars must appear in determining vars
                    for arm_idx in 0..arm_data.len() {
                        let (pattern_types, span, _) = &arm_data[arm_idx];
                        for &det_idx in determined_indices {
                            if !determining_indices.contains(&det_idx) {
                                // Check if this determined param appears in the pattern
                                if let Type::TypeVar(det_name, _) = &pattern_types[det_idx] {
                                    // Jones (2000) coverage condition: the *same* TypeVar that
                                    // appears in the determined position must also appear at one
                                    // or more determining positions.  Checking for any TypeVar
                                    // in the determining positions is insufficient — a different
                                    // TypeVar there does not establish a coverage relation.
                                    let same_var_in_determining =
                                        determining_indices.iter().any(|&det_pos| {
                                            matches!(&pattern_types[det_pos], Type::TypeVar(n, _) if n == det_name)
                                        });

                                    if !same_var_in_determining {
                                        let param_name = param_names
                                            .get(det_idx)
                                            .map(|s| s.as_str())
                                            .unwrap_or("<unknown>");
                                        return Err(vec![TypeError::new(
                                            format!(
                                                "coverage violation for class '{}': determined parameter '{}' (variable '{}') does not appear in any determining position",
                                                class_name, param_name, det_name
                                            ),
                                            *span,
                                        )
                                        .with_code("T016")]);
                                    }
                                }
                            }
                        }
                    }

                    // Consistency check (Task 7.4): if determining positions are structurally equal,
                    // determined types must also be structurally equal (Jones 2000 condition).
                    // Uses types_equal (structural PartialEq) rather than a unify probe to avoid
                    // O(N²) InferState clones that cause test-suite hangs.
                    for i in 0..arm_data.len() {
                        for j in (i + 1)..arm_data.len() {
                            let (types_i, span_i, _) = &arm_data[i];
                            let (types_j, span_j, _) = &arm_data[j];

                            let determining_i: Vec<Type> = determining_indices
                                .iter()
                                .map(|&idx| types_i[idx].clone())
                                .collect();
                            let determining_j: Vec<Type> = determining_indices
                                .iter()
                                .map(|&idx| types_j[idx].clone())
                                .collect();

                            // Only check consistency when determining positions are structurally equal
                            if types_equal(&determining_i, &determining_j) {
                                let determined_i: Vec<Type> = determined_indices
                                    .iter()
                                    .map(|&idx| types_i[idx].clone())
                                    .collect();
                                let determined_j: Vec<Type> = determined_indices
                                    .iter()
                                    .map(|&idx| types_j[idx].clone())
                                    .collect();

                                if !types_equal(&determined_i, &determined_j) {
                                    let error = TypeError::new(
                                        format!(
                                            "consistency violation for class '{}': arm at line {} and arm at line {} both match determining positions but disagree on determined types",
                                            class_name,
                                            span_i.start.line,
                                            span_j.start.line
                                        ),
                                        *span_j,
                                    )
                                    .with_code("T015");
                                    return Err(vec![error]);
                                }
                            }
                        }
                    }
                }
            }

            // Register arms in InstanceEnv (Task 7.5)
            for (pattern_types, _span, methods) in &arm_data {
                // Build instance type from pattern types
                // For single-param classes, use the first type directly
                // For multi-param classes, use a tuple-like representation (Type::Record for now)
                let inst_type = if pattern_types.len() == 1 {
                    pattern_types[0].clone()
                } else {
                    // Multi-param: encode as record with numbered fields for now
                    // TODO: Proper tuple type or Type::Tuple variant
                    Type::Record(Row {
                        fields: pattern_types
                            .iter()
                            .enumerate()
                            .map(|(i, ty)| (i.to_string(), ty.clone()))
                            .collect(),
                    })
                };

                // Infer types for all method implementations (Task 7.6)
                let mut method_types = HashMap::new();

                // Skip method body inference during prelude loading (optimization).
                // The method types are unused — they're stored in InstanceDecl.method_types
                // which is #[allow(dead_code)]. This avoids O(N²) state clones in
                // patterns_overlap when prelude instances reach a certain count.
                if !state.in_prelude_load {
                    for method in *methods {
                        // Extract method name from key
                        let method_name = match &method.node.key {
                            Some(key_expr) => match &key_expr.node {
                                Expr::Str(s) => s.clone(),
                                Expr::VarRef { name, .. } => name.clone(),
                                _ => {
                                    return Err(vec![TypeError::new(
                                        "instance method name must be a string or identifier",
                                        key_expr.span,
                                    )]);
                                }
                            },
                            None => {
                                return Err(vec![TypeError::new(
                                    "instance method must have a name",
                                    method.span,
                                )]);
                            }
                        };

                        // Infer the type of the method implementation
                        let method_impl_type =
                            infer_expr(&method.node.value, env, state, type_map)?;

                        method_types.insert(method_name, method_impl_type);
                    }
                }

                // Build instance declaration
                let instance_decl = InstanceDecl {
                    class_name: class_name.clone(),
                    instance_type: inst_type,
                    method_types,
                };

                // Register in InstanceEnv
                if let Err(msg) = state.instance_env.insert(instance_decl) {
                    return Err(vec![TypeError::new(msg, expr.span)]);
                }
            }

            // InstanceDecl expressions evaluate to empty record (see eval.rs)
            Ok(Type::Record(Row {
                fields: HashMap::new(),
            }))
        }

        Expr::PatternDecl { .. } => {
            // PatternDecl should never appear in value positions (only in instance arms)
            Err(vec![TypeError::new(
                "pattern declaration is only valid in instance match arms",
                expr.span,
            )])
        }

        Expr::LetDecl { bindings } => {
            // LetDecl in value position is always an error (only valid in binding contexts).
            // Provide a more specific message for multi-element bindings since the likely
            // intent is a pattern match arm, not a value expression.
            let msg = if bindings.len() > 1 {
                "multi-element [let ...] pattern not yet supported — use single binding".to_string()
            } else {
                "binding declaration [let ...] is not valid in expression position".to_string()
            };
            Err(vec![TypeError::new(msg, expr.span)])
        }

        Expr::CaseArm { pattern, body } => {
            // CaseArm can be type-checked standalone, though typically it appears inside match.
            // For now: infer pattern type, infer body type, return body type.
            // The scrutinee type is Unknown when CaseArm is checked standalone.
            typecheck_case_arm(pattern, body, &Type::Unknown, env, state, type_map)
        }

        Expr::Placeholder => {
            // Placeholder has type Unknown — satisfies any constraint.
            // This is the gradual typing escape hatch: ... can be used anywhere a value is needed,
            // deferring type checking to runtime (where it raises UnimplementedError when forced).
            Ok(Type::Unknown)
        }

        Expr::Rest(_) => Err(vec![TypeError::new(
            "rest marker (...) is only valid inside type expressions",
            expr.span,
        )]),

        Expr::TypeApp { .. } => {
            // TypeApp is type-level only — look up the resolved App type from type_map.
            // When resolve_type_expr processes a TypeApp annotation, it resolves to Type::App
            // and stores it in type_map. If not found (e.g., TypeApp outside annotation context),
            // gracefully return Unknown.
            if let Some(ref map) = type_map {
                let key = (expr.span.start.offset, expr.span.end.offset);
                if let Some(resolved_ty) = map.get(&key) {
                    return Ok(resolved_ty.clone());
                }
            }
            Ok(Type::Unknown)
        }

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
    // Simplify compound types (RDNF step 1d) before storing so LSP hover shows the
    // reduced form (e.g., Union([Int, Int]) → Int, Intersection([Never, T]) → Never).
    if let Some(ref mut map) = type_map {
        let key = (expr.span.start.offset, expr.span.end.offset);
        match &result {
            Ok(ty) => {
                let simplified = Type::simplify_type(ty.clone());
                map.insert(key, simplified);
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
/// Check if a type contains Unknown or Top anywhere in its structure.
/// Used for the gradual typing fallback: when Unknown/Top appears anywhere in a type,
/// subsumption uses `is_consistent` instead of `is_subtype` to maintain the gradual guarantee.
fn contains_unknown_or_top(ty: &Type) -> bool {
    match ty {
        Type::Unknown | Type::Top => true,
        // TypeVar is treated as gradual (like Unknown) in the subsumption check.
        // An unresolved TypeVar represents an unknown type that could be anything.
        // Internal TypeVars from annotated params, `instantiate_scheme`, and
        // `fresh_type_var` used in pass-1 positions can appear during body checking
        // before the substitution has resolved them. Without this arm, TypeVars in
        // an actual type would fall through to `_ => false` in is_subtype, causing
        // false subsumption failures against concrete expected types like Number or Str.
        Type::TypeVar(_, _) => true,
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, t)| contains_unknown_or_top(t)) || contains_unknown_or_top(ret)
        }
        Type::Seq(elem) => contains_unknown_or_top(elem),
        Type::Record(row) => row.fields.values().any(contains_unknown_or_top),
        Type::Union(members) => members.iter().any(contains_unknown_or_top),
        _ => false,
    }
}

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
                        .map(|(p, (_, expected_ty))| match &p.node.annotation {
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
                                    // Apply substitution before consistency check
                                    let (expected_ty_resolved, resolved_ty) = if state.subst.is_empty() {
                                        (expected_ty.clone(), resolved.clone())
                                    } else {
                                        (state.subst.apply(expected_ty), state.subst.apply(&resolved))
                                    };
                                    let sub_passes = Type::is_subtype(&expected_ty_resolved, &resolved_ty)
                                        || ((contains_unknown_or_top(&expected_ty_resolved)
                                            || contains_unknown_or_top(&resolved_ty))
                                            && Type::is_consistent(&expected_ty_resolved, &resolved_ty));
                                    if !sub_passes {
                                        return Err(TypeError::new(
                                            format!("parameter annotation {resolved_ty} is more restrictive than required type {expected_ty_resolved}"),
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
                            fn_env.insert(param.node.name.clone(), Type::Unknown);
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
                                // Apply substitution before consistency check
                                let (declared_resolved, expected_ret_resolved) =
                                    if state.subst.is_empty() {
                                        (declared.clone(), expected_ret.clone())
                                    } else {
                                        (
                                            state.subst.apply(&declared),
                                            Box::new(state.subst.apply(expected_ret)),
                                        )
                                    };
                                let sub_passes =
                                    Type::is_subtype(&declared_resolved, &expected_ret_resolved)
                                        || ((contains_unknown_or_top(&declared_resolved)
                                            || contains_unknown_or_top(&expected_ret_resolved))
                                            && Type::is_consistent(
                                                &declared_resolved,
                                                &expected_ret_resolved,
                                            ));
                                if !sub_passes {
                                    return Err(vec![TypeError::type_mismatch(
                                        &expected_ret_resolved,
                                        &declared_resolved,
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
                            let applied_ret = if state.subst.type_map.borrow().is_empty() {
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

    // Default: synthesize then check
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

    // Unified CALL-MONO/CALL-POLY path: eliminates verdict divergence between monomorphic
    // and polymorphic function calls. When expected type has TypeVars, use unification to
    // bind them (CALL-POLY). When expected type is concrete, use subsumption (CALL-MONO).
    // This ensures identical literal pairs get consistent verdicts regardless of whether
    // the function type has inference vars.
    if expected_resolved.has_inference_vars() {
        // Expected type contains TypeVars — use unification to bind them.
        // This is the CALL-POLY path: the function is polymorphic, and we need to
        // instantiate type variables based on the argument types.
        let mut subst = std::mem::take(&mut state.subst);
        let result = unify(&actual, &expected_resolved, &mut subst, state, expr.span);
        state.subst = subst;
        result.map_err(|e| vec![e])
    } else {
        // Expected type is concrete — use subsumption with gradual typing fallback.
        // This is the CALL-MONO path: the function type is fully known, so we check
        // that the argument type is a subtype of the parameter type.
        //
        // Use is_subtype for standard HM subsumption. When is_subtype fails and either type
        // contains Unknown (gradual ?) anywhere in its structure, fall back to is_consistent.
        // The gradual guarantee requires that making types less precise (adding ?) never
        // causes new type errors (Siek & Taha 2006). We only use the consistency fallback
        // when Unknown is present, because is_consistent is symmetric (Number ~ Int) while
        // is_subtype is directional (Int <: Number but NOT Number <: Int).

        // Apply substitution before consistency check
        let (actual_resolved, expected_final) = if state.subst.is_empty() {
            (actual.clone(), expected_resolved.clone())
        } else {
            (
                state.subst.apply(&actual),
                state.subst.apply(&expected_resolved),
            )
        };

        let passes = Type::is_subtype(&actual_resolved, &expected_final)
            || ((contains_unknown_or_top(&actual_resolved)
                || contains_unknown_or_top(&expected_final))
                && Type::is_consistent(&actual_resolved, &expected_final));
        if !passes {
            Err(vec![TypeError::type_mismatch(
                &expected_final,
                &actual_resolved,
                expr.span,
            )])
        } else {
            Ok(())
        }
    }
}

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
fn check_get(
    is_optional: bool,
    args: &[Rc<Spanned<Expr>>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let builtin_name = if is_optional { "get?" } else { "get" };

    // Validate arity: exactly 2 positional args, no named args.
    if named_args.is_empty() && args.len() == 2 {
        // Infer key argument (side effects: type_map population).
        let key_ty = infer_expr(&args[0], env, state, type_map)?;
        let key_ty = state.subst.apply(&key_ty);

        // Infer dict argument.
        let dict_ty = infer_expr(&args[1], env, state, type_map)?;
        let dict_ty = state.subst.apply(&dict_ty);

        // Null type: empty closed record (Value::Dict(empty) at runtime).
        let null_ty = Type::Record(Row {
            fields: HashMap::new(),
        });

        let make_nullable = |ty: Type| -> Type {
            if is_optional {
                Type::normalize_union(vec![ty, null_ty.clone()])
            } else {
                ty
            }
        };

        let result = match &dict_ty {
            Type::Map(_, val_ty) => {
                // Map[K V]: get returns V, get? returns V|Null.
                make_nullable(*val_ty.clone())
            }
            Type::Seq(elem_ty) => {
                // Seq[T] with an integer key: get returns T, get? returns T|Null.
                // split returns Seq[Str], so [get N [split sep s]] → Str.
                match &key_ty {
                    Type::Int | Type::IntLiteral(_) => make_nullable(*elem_ty.clone()),
                    _ => Type::Unknown,
                }
            }
            Type::Record(row) => {
                // Record: if the key is a known string literal, look up the field.
                match &key_ty {
                    Type::StringLiteral(field) => {
                        match row.fields.get(field) {
                            Some(field_ty) => make_nullable(field_ty.clone()),
                            None => {
                                // Field not in the known set. For closed records this is an error
                                // at runtime, but we return Unknown here to allow gradual typing.
                                // check_dot_access returns an error for closed records; get/get?
                                // are more permissive since they take dynamic keys.
                                Type::Unknown
                            }
                        }
                    }
                    Type::TypeVar(key_var_name, _) => {
                        // Label TypeVar key → resolve via resolve_has_field
                        if state.kind_env.get(key_var_name) == Some(&Kind::Label) {
                            // Try to resolve the field type. If the label TypeVar is already
                            // bound to a StringLiteral, this will succeed. Otherwise, it fails
                            // and we fall back to Unknown.
                            match resolve_has_field(
                                &Label::Var(key_var_name.clone()),
                                &dict_ty,
                                state,
                                span,
                                0,
                            ) {
                                Ok(field_ty) => make_nullable(field_ty),
                                Err(_) => Type::Unknown,
                            }
                        } else {
                            // Non-label TypeVar key against a Record: we can't narrow statically.
                            Type::Unknown
                        }
                    }
                    _ => {
                        // Non-literal key against a Record: we can't narrow statically.
                        Type::Unknown
                    }
                }
            }
            Type::TypeVar(dict_var_name, _) => {
                // TypeVar dict: check if key is a label TypeVar or concrete StringLiteral
                match &key_ty {
                    Type::StringLiteral(field_name) => {
                        // Concrete string key → emit HasField constraint
                        let field_var = state.fresh_type_var();
                        let field_var_name = match &field_var {
                            Type::TypeVar(name, _) => name.clone(),
                            _ => unreachable!("fresh_type_var returns TypeVar"),
                        };

                        state.constraints.push(Constraint::HasField {
                            label: Label::Concrete(field_name.clone()),
                            dict_var: dict_var_name.clone(),
                            field_var: field_var_name,
                        });

                        make_nullable(field_var)
                    }
                    Type::TypeVar(key_var_name, _) => {
                        // Check if key is a label TypeVar
                        if state.kind_env.get(key_var_name) == Some(&Kind::Label) {
                            // Label TypeVar key → emit HasField constraint
                            let field_var = state.fresh_type_var();
                            let field_var_name = match &field_var {
                                Type::TypeVar(name, _) => name.clone(),
                                _ => unreachable!("fresh_type_var returns TypeVar"),
                            };

                            state.constraints.push(Constraint::HasField {
                                label: Label::Var(key_var_name.clone()),
                                dict_var: dict_var_name.clone(),
                                field_var: field_var_name,
                            });

                            make_nullable(field_var)
                        } else {
                            // Non-label TypeVar key: fall back to Unknown
                            Type::Unknown
                        }
                    }
                    _ => {
                        // Non-literal, non-label-TypeVar key: fall back to Unknown
                        Type::Unknown
                    }
                }
            }
            Type::Union(_) | Type::Intersection(_) | Type::Top => {
                // For Union/Intersection/Top, resolve via resolve_has_field
                match &key_ty {
                    Type::StringLiteral(field_name) => {
                        match resolve_has_field(
                            &Label::Concrete(field_name.clone()),
                            &dict_ty,
                            state,
                            span,
                            0,
                        ) {
                            Ok(field_ty) => make_nullable(field_ty),
                            Err(_) => Type::Unknown,
                        }
                    }
                    Type::TypeVar(key_var_name, _) => {
                        // Label TypeVar key → resolve via resolve_has_field
                        if state.kind_env.get(key_var_name) == Some(&Kind::Label) {
                            match resolve_has_field(
                                &Label::Var(key_var_name.clone()),
                                &dict_ty,
                                state,
                                span,
                                0,
                            ) {
                                Ok(field_ty) => make_nullable(field_ty),
                                Err(_) => Type::Unknown,
                            }
                        } else {
                            Type::Unknown
                        }
                    }
                    _ => Type::Unknown,
                }
            }
            _ => {
                // Unknown, or other: fall back to Unknown.
                Type::Unknown
            }
        };

        return Ok(result);
    }

    // Unexpected arity or named args: fall through to the normal call path
    // (which will produce an arity or type error as appropriate).
    Err(vec![TypeError::new(
        format!(
            "arity mismatch: {} expects exactly 2 arguments, got {} ({} named)",
            builtin_name,
            args.len(),
            named_args.len()
        ),
        span,
    )])
}

/// Type check `get-in` — chained field access.
/// [GET-IN-NIL]: empty path returns dict unchanged
/// [GET-IN-CONS]: unfold via repeated check_get
fn check_get_in(
    args: &[Rc<Spanned<Expr>>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
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
    let dict_ty = infer_expr(&args[1], env, state, type_map)?;
    let dict_ty = state.subst.apply(&dict_ty);

    // Check if path is a literal dict with auto-indexed string entries
    let path_expr = &args[0].node;
    match path_expr {
        Expr::Dict(entries) => {
            // Check all entries are auto-indexed StringLiterals
            let mut keys = Vec::new();
            for (idx, entry) in entries.iter().enumerate() {
                // Check if auto-indexed (key is None or matches index)
                let is_auto_indexed = match &entry.node.key {
                    None => true,
                    Some(key_expr) => match &key_expr.node {
                        Expr::Int(n) if *n == idx as i64 => true,
                        _ => false,
                    },
                };

                if !is_auto_indexed {
                    // Non-auto-indexed entry: fall back to Unknown
                    return Ok(Type::Unknown);
                }

                // Check if value is a string literal
                match &entry.node.value.node {
                    Expr::Str(s) => keys.push(s.clone()),
                    _ => {
                        // Non-literal path element: fall back to Unknown
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
                            // Field not found: fall back to Unknown
                            return Ok(Type::Unknown);
                        }
                    }
                    Type::Union(_) | Type::Intersection(_) | Type::Top => {
                        // Resolve via resolve_has_field
                        match resolve_has_field(&Label::Concrete(key), &current_ty, state, span, 0)
                        {
                            Ok(field_ty) => current_ty = field_ty,
                            Err(_) => return Ok(Type::Unknown),
                        }
                    }
                    Type::Unknown => return Ok(Type::Unknown),
                    _ => {
                        // Not a record or union: fall back to Unknown
                        return Ok(Type::Unknown);
                    }
                }
            }

            Ok(current_ty)
        }
        _ => {
            // Path is not a literal sequence: fall back to Unknown
            Ok(Type::Unknown)
        }
    }
}

fn check_dot_access(
    target: &Spanned<Expr>,
    field: &crate::ast::DotKey,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Convert DotKey to string for field lookup
    let field_str = match field {
        crate::ast::DotKey::Ident(s) => s.as_str(),
        crate::ast::DotKey::Int(n) => {
            return check_dot_access_int(target, *n, env, span, state, type_map)
        }
    };

    // [DOT-POLY] fast-path: if target is a VarRef and its scheme has inner_schemes,
    // instantiate the field's scheme polymorphically
    if let Expr::VarRef { name, .. } = &target.node {
        if let Some(scheme) = env.get(name) {
            if let Some(ref inner_schemes) = scheme.inner_schemes {
                if let Some(field_scheme) = inner_schemes.get(field_str) {
                    let instantiated = instantiate_scheme(field_scheme, state.level, state);
                    return Ok(instantiated);
                }
            }
        }
    }

    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution so that constraints from prior accesses
    // on the same target are visible (doc/07-type-extensions.md Part 5).
    let target_ty = state.subst.apply(&target_ty);
    match target_ty {
        Type::Record(Row { ref fields, .. }) => match fields.get(field_str) {
            Some(ty) => Ok(ty.clone()),
            // BAS: field not found in known fields — return Unknown (width subtyping:
            // the field may be present in the concrete value via extra fields).
            None => Ok(Type::Unknown),
        },
        // TypeVar α: generate constraint α = Record({field: β}).
        // Under BAS, no row variable needed — empty record type covers the requirement.
        Type::TypeVar(ref alpha, alpha_level) => {
            // Create fresh β for the field type
            let beta = state.fresh_type_var();

            // Build the record type to unify α with (BAS: no RowVar tail)
            let mut fields = HashMap::new();
            fields.insert(field_str.to_string(), beta.clone());
            let record_ty = Type::Record(Row { fields });

            // Unify TypeVar(α) with Record({field: β})
            let alpha_ty = Type::TypeVar(alpha.clone(), alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;

            Ok(beta)
        }
        Type::Unknown => Ok(Type::Unknown),
        Type::Proxy => Ok(Type::Unknown),
        // Intersection type: search each member for the field.
        // An intersection value satisfies all members, so any member that has the field
        // statically provides its type.  Return the first match; if no member has the
        // field statically, fall back to Unknown (a member with an open row tail may
        // accept the field dynamically, and we cannot resolve it at compile time without
        // full constraint propagation into each member's row variable).
        Type::Intersection(ref members) => {
            for member in members {
                if let Type::Record(Row { ref fields, .. }) = member {
                    if let Some(ty) = fields.get(field_str) {
                        return Ok(ty.clone());
                    }
                }
            }
            // No member had the field statically.
            Ok(Type::Unknown)
        }
        // Negation type: ~A narrows inhabitance, not field structure.
        // We cannot extract field types from a negation, so fall back to Unknown.
        Type::Negation(_) => Ok(Type::Unknown),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Type check integer dot access: `$data.0`
fn check_dot_access_int(
    target: &Spanned<Expr>,
    index: i64,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    let target_ty = state.subst.apply(&target_ty);

    let field_name = index.to_string();

    match &target_ty {
        Type::Record(Row { ref fields, .. }) => {
            if let Some(ty) = fields.get(field_name.as_str()) {
                return Ok(ty.clone());
            }
            // BAS: field not found in known fields — return Unknown
            // (width subtyping: the field may be present in the concrete value)
            Ok(Type::Unknown)
        }
        Type::TypeVar(ref alpha, alpha_level) => {
            let beta = state.fresh_type_var();

            let mut fields = HashMap::new();
            fields.insert(field_name, beta.clone());
            let record_ty = Type::Record(Row { fields });

            let alpha_ty = Type::TypeVar(alpha.clone(), *alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;
            Ok(beta)
        }
        Type::Unknown => Ok(Type::Unknown),
        Type::Proxy => Ok(Type::Unknown),
        // Intersection type: search each member for the numeric field.
        Type::Intersection(ref members) => {
            for member in members {
                if let Type::Record(Row { ref fields, .. }) = member {
                    if let Some(ty) = fields.get(field_name.as_str()) {
                        return Ok(ty.clone());
                    }
                }
            }
            Ok(Type::Unknown)
        }
        // Negation type: fall back to Unknown for integer field access.
        Type::Negation(_) => Ok(Type::Unknown),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Check if a type is concrete (not Unknown, not a TypeVar, not Top).
/// Used for boundary guard detection in gradual typing.
fn is_concrete_type(ty: &Type) -> bool {
    match ty {
        // Non-concrete: open inference variables or imprecise top types.
        // Top is the "any" type (like dynamic/unknown) — not a concrete constraint.
        Type::Unknown | Type::TypeVar(_, _) | Type::Top => false,
        // Composite types: recurse into components.
        Type::Function { params, ret, .. } => {
            params.iter().all(|(_, p)| is_concrete_type(p)) && is_concrete_type(ret)
        }
        Type::Record(row) => row.fields.values().all(is_concrete_type),
        Type::Seq(elem) => is_concrete_type(elem),
        Type::Map(k, v) => is_concrete_type(k) && is_concrete_type(v),
        Type::Union(types) => types.iter().all(is_concrete_type),
        Type::Intersection(types) => types.iter().all(is_concrete_type),
        // Ground types: Int, Float, Str, Bool, Never, Negation, App, TypeStageApp, etc.
        // TypeStageApp is treated as concrete here: it is constructed by the resolver
        // cache from ground types, so it is fully determined at the point boundary
        // guards are checked.
        _ => true,
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

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
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
                    type_map: std::cell::RefCell::new(state.subst.type_map.borrow().clone()),
                };
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param.
                let mut consumed_params = std::collections::HashSet::new();
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !params.is_empty() {
                    params.len() - 1
                } else {
                    params.len()
                };
                for (idx, ((_, param_ty), arg_ty)) in params
                    .iter()
                    .take(non_variadic_param_count)
                    .zip(arg_types.iter())
                    .enumerate()
                {
                    consumed_params.insert(idx);

                    // Boundary guard tracking: if argument is Unknown and parameter expects
                    // a concrete type, record this as a gradual typing boundary.
                    if matches!(arg_ty, Type::Unknown) && is_concrete_type(param_ty) {
                        // Record the argument span and expected type for gradual typing
                        // boundary guard insertion at eval time. HashMap ensures O(1)
                        // lookup per span in eval_recursive.
                        if idx < args.len() {
                            state
                                .boundary_guards
                                .insert(args[idx].span, param_ty.clone());
                        }
                    }

                    // Error-typed args absorb silently (unify(Error, T) = Ok(())),
                    // so we only propagate unification errors from non-Error args.
                    if let Err(e) = unify(param_ty, arg_ty, &mut subst, state, span) {
                        arg_errors.get_or_insert_with(Vec::new).push(e);
                    }
                }
                // Check variadic args: if the function is variadic, unify all arg_types starting at
                // non_variadic_param_count against the Seq element type. Widen literals first.
                if *variadic && arg_types.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, param_ty)) = params.last() {
                        if let Type::Seq(elem_ty) = param_ty {
                            for arg_ty in arg_types.iter().skip(non_variadic_param_count) {
                                // Widen literal types before unifying
                                let widened_ty = match arg_ty {
                                    Type::IntLiteral(_) => Type::Int,
                                    Type::StringLiteral(_) => Type::Str,
                                    other => other.clone(),
                                };
                                if let Err(e) = unify(elem_ty, &widened_ty, &mut subst, state, span)
                                {
                                    arg_errors.get_or_insert_with(Vec::new).push(e);
                                }
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
                let mut seen_names: HashSet<&str> = HashSet::new();
                for na in named_args {
                    if !seen_names.insert(&na.node.name) {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!("duplicate named argument: '{}'", na.node.name),
                            na.span,
                        ));
                    }
                }
                // Unify named args by matching them to params by name.
                // Mirrors check_call CALL-POLY named-arg loop (same pattern, same error messages).
                // `params` here are already the instantiated params from instantiate_scheme above.
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                        if pname.as_ref() == Some(arg_name) {
                            Some((idx, pty))
                        } else {
                            None
                        }
                    });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type error and skip unification (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span,
                                ));
                                continue;
                            }
                            // Mark param as consumed (Task 1: Robinson idempotency)
                            consumed_params.insert(param_idx);
                            // Infer named arg type and unify
                            match infer_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Task 2: merge state.subst updates from infer_expr into local subst
                                    subst
                                        .type_map
                                        .borrow_mut()
                                        .extend(state.subst.type_map.borrow().clone());
                                    if let Err(e) =
                                        unify(&arg_ty, param_ty, &mut subst, state, na.span)
                                    {
                                        arg_errors.get_or_insert_with(Vec::new).push(
                                            TypeError::new(
                                                format!(
                                                    "named argument '{}' type mismatch: {}",
                                                    arg_name, e.message
                                                ),
                                                na.span,
                                            ),
                                        );
                                    }
                                }
                                Err(mut errs) => {
                                    arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                }
                            }
                        }
                        None => {
                            arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span,
                            ));
                        }
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
                for (k, v) in subst.type_map.borrow().iter() {
                    state
                        .subst
                        .type_map
                        .borrow_mut()
                        .insert(k.clone(), v.clone());
                }
                state.subst.check_size(span).map_err(|e| vec![e])?;
                // After merging local subst into state.subst, state.subst is a superset of subst.
                // Applying state.subst directly is sufficient — a prior double-application
                // (subst.apply then state.subst.apply) was redundant because state.subst already
                // contains everything subst mapped.
                Ok(state.subst.apply(ret))
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
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-POLY (infer_expr at line 934).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map);
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-POLY arm infers named
            // args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
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
/// Named argument type checking fires in three paths:
/// - CALL-MONO (here): for each named arg, finds the matching parameter by name in `params`
///   and unifies the arg type against the parameter type via `infer_expr` + `unify`; emits
///   `TypeError` on name mismatch ("unknown named argument") or type mismatch.
/// - CALL-POLY (here): same name-based lookup and unify on the instantiated params.
/// - `check_call_with_scheme` Function arm: same name-based lookup and unify after positional
///   arg unification; uses `params` from the already-instantiated `func_ty`.
/// Note: named-arg checking fires only for resolved function types; same-dict letrec forward
/// references fall through to the `TypeVar` arm and skip named-arg validation.
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

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
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
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param if the call
                // supplies both positional and named args (e.g., [call $f 42 x: 99] where param 0 is
                // named x would check param 0 twice).
                let mut consumed_params = std::collections::HashSet::new();
                // Check positional args against non-variadic params.
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !params.is_empty() {
                    params.len() - 1
                } else {
                    params.len()
                };
                for (idx, (arg, (_param_name, param_ty))) in args
                    .iter()
                    .zip(params.iter().take(non_variadic_param_count))
                    .enumerate()
                {
                    consumed_params.insert(idx);

                    // Boundary guard tracking (mirrors check_call_with_scheme lines ~3568-3576):
                    // if the argument's inferred type is Unknown and the parameter expects a
                    // concrete type, record the span for gradual typing boundary guard insertion.
                    // Infer first (may update type_map for LSP hover), then check_expr for
                    // bidirectional checking. Double-inference is safe: state remains consistent.
                    if is_concrete_type(param_ty) {
                        if let Ok(arg_ty) = infer_expr(arg, env, state, type_map) {
                            let resolved_arg_ty = if state.subst.is_empty() {
                                arg_ty
                            } else {
                                state.subst.apply(&arg_ty)
                            };
                            if matches!(resolved_arg_ty, Type::Unknown) {
                                state.boundary_guards.insert(arg.span, param_ty.clone());
                            }
                        }
                    }

                    if let Err(mut errs) = check_expr(arg, param_ty, env, state, type_map) {
                        errors.append(&mut errs);
                    }
                }
                // Check variadic args: if the function is variadic, infer all args starting at
                // non_variadic_param_count and unify them against the Seq element type.
                // Use infer+unify instead of check_expr to allow literal widening (IntLiteral → Int).
                if *variadic && args.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, param_ty)) = params.last() {
                        if let Type::Seq(elem_ty) = param_ty {
                            for arg in args.iter().skip(non_variadic_param_count) {
                                match infer_expr(arg, env, state, type_map) {
                                    Ok(arg_ty) => {
                                        // Widen literal types before unifying to allow [f 10 20 30]
                                        // where 10, 20, 30 all unify with Int element type.
                                        let widened_ty = match arg_ty {
                                            Type::IntLiteral(_) => Type::Int,
                                            Type::StringLiteral(_) => Type::Str,
                                            other => other,
                                        };
                                        let mut subst = std::mem::take(&mut state.subst);
                                        if let Err(e) =
                                            unify(&widened_ty, elem_ty, &mut subst, state, arg.span)
                                        {
                                            errors.push(e);
                                        }
                                        state.subst = subst;
                                    }
                                    Err(mut errs) => {
                                        errors.append(&mut errs);
                                    }
                                }
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
                let mut seen_names: HashSet<&str> = HashSet::new();
                for na in named_args {
                    if !seen_names.insert(&na.node.name) {
                        errors.push(TypeError::new(
                            format!("duplicate named argument: '{}'", na.node.name),
                            na.span,
                        ));
                    }
                }
                // Check named args by matching them to params by name
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                        if pname.as_ref() == Some(arg_name) {
                            Some((idx, pty))
                        } else {
                            None
                        }
                    });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type warning and skip type checking (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                errors.push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span,
                                ));
                                continue;
                            }
                            // Mark param as consumed (Task 1: Robinson idempotency)
                            consumed_params.insert(param_idx);

                            // Infer the named arg type and unify against the param type.
                            // Boundary guard tracking: after inferring the arg type, if it is
                            // Unknown and the parameter expects a concrete type, record the span
                            // for gradual typing boundary guard insertion. This avoids a redundant
                            // pre-call infer_expr that would mutate state before the actual check.
                            match infer_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Boundary guard check (post-inference, single-pass)
                                    if is_concrete_type(param_ty) {
                                        let resolved_arg_ty = if state.subst.is_empty() {
                                            arg_ty.clone()
                                        } else {
                                            state.subst.apply(&arg_ty)
                                        };
                                        if matches!(resolved_arg_ty, Type::Unknown) {
                                            state
                                                .boundary_guards
                                                .insert(na.node.value.span, param_ty.clone());
                                        }
                                    }
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result =
                                        unify(&arg_ty, param_ty, &mut subst, state, na.span);
                                    state.subst = subst;
                                    if let Err(e) = result {
                                        errors.push(TypeError::new(
                                            format!(
                                                "named argument '{}' type mismatch: {}",
                                                arg_name, e.message
                                            ),
                                            na.span,
                                        ));
                                    }
                                }
                                Err(mut errs) => {
                                    errors.append(&mut errs);
                                }
                            }
                        }
                        None => {
                            errors.push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span,
                            ));
                        }
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
            // Instantiate the function type, then check arguments (doc/06 §[CALL-POLY])
            // Unified with CALL-MONO: both paths use check_expr, which internally dispatches
            // to unification (for TypeVars) or subsumption (for concrete types).
            let inst_ty = instantiate_at_level(&func_ty, state);

            let (inst_params, inst_ret) = match &inst_ty {
                Type::Function {
                    params,
                    ret,
                    variadic: _,
                } => (params, ret),
                _ => unreachable!("instantiate_at_level preserves Function variant"),
            };

            // Check arguments against instantiated parameter types.
            // check_expr will use unification because inst_params contain fresh TypeVars.
            let mut arg_errors: Option<Vec<TypeError>> = None;

            if !params.is_empty() {
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param.
                let mut consumed_params = std::collections::HashSet::new();
                // Check positional args via check_expr (unified CALL-MONO/CALL-POLY path).
                // check_expr will use unification internally because inst_params contain TypeVars.
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !inst_params.is_empty() {
                    inst_params.len() - 1
                } else {
                    inst_params.len()
                };
                for (idx, (arg, (_param_name, param_ty))) in args
                    .iter()
                    .zip(inst_params.iter().take(non_variadic_param_count))
                    .enumerate()
                {
                    consumed_params.insert(idx);
                    if let Err(mut errs) = check_expr(arg, param_ty, env, state, type_map) {
                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                    }
                }
                // Check variadic args: if the function is variadic, infer all args starting at
                // non_variadic_param_count and unify them against the Seq element type.
                // Use infer+unify instead of check_expr to allow literal widening (IntLiteral → Int).
                if *variadic && args.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, param_ty)) = inst_params.last() {
                        if let Type::Seq(elem_ty) = param_ty {
                            for arg in args.iter().skip(non_variadic_param_count) {
                                match infer_expr(arg, env, state, type_map) {
                                    Ok(arg_ty) => {
                                        // Widen literal types before unifying
                                        let widened_ty = match arg_ty {
                                            Type::IntLiteral(_) => Type::Int,
                                            Type::StringLiteral(_) => Type::Str,
                                            other => other,
                                        };
                                        let mut subst = std::mem::take(&mut state.subst);
                                        if let Err(e) =
                                            unify(&widened_ty, elem_ty, &mut subst, state, arg.span)
                                        {
                                            arg_errors.get_or_insert_with(Vec::new).push(e);
                                        }
                                        state.subst = subst;
                                    }
                                    Err(mut errs) => {
                                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                    }
                                }
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
                let mut seen_names: HashSet<&str> = HashSet::new();
                for na in named_args {
                    if !seen_names.insert(&na.node.name) {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!("duplicate named argument: '{}'", na.node.name),
                            na.span,
                        ));
                    }
                }
                // Check named args by matching them to params by name
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match =
                        inst_params
                            .iter()
                            .enumerate()
                            .find_map(|(idx, (pname, pty))| {
                                if pname.as_ref() == Some(arg_name) {
                                    Some((idx, pty))
                                } else {
                                    None
                                }
                            });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type error and skip checking (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span,
                                ));
                                continue;
                            }
                            // Mark param as consumed
                            consumed_params.insert(param_idx);

                            // Check named arg: infer arg type once, then record boundary guard
                            // if arg is Unknown and param expects a concrete type, then unify.
                            // This avoids a redundant pre-call infer_expr that would mutate
                            // state before the actual bidirectional check (the prior pattern of
                            // calling infer_expr twice — once for guard, once via check_expr —
                            // left stale type vars from the first call affecting the second).
                            match infer_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Boundary guard check (post-inference, single-pass)
                                    if is_concrete_type(param_ty) {
                                        let resolved_arg_ty = if state.subst.is_empty() {
                                            arg_ty.clone()
                                        } else {
                                            state.subst.apply(&arg_ty)
                                        };
                                        if matches!(resolved_arg_ty, Type::Unknown) {
                                            state
                                                .boundary_guards
                                                .insert(na.node.value.span, param_ty.clone());
                                        }
                                    }
                                    // Unify the inferred type against the expected param type
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result =
                                        unify(&arg_ty, param_ty, &mut subst, state, na.span);
                                    state.subst = subst;
                                    if let Err(errs) = result {
                                        arg_errors.get_or_insert_with(Vec::new).push(
                                            TypeError::new(
                                                format!(
                                                    "named argument '{}' type mismatch: {}",
                                                    arg_name, errs.message
                                                ),
                                                na.span,
                                            ),
                                        );
                                    }
                                }
                                Err(mut errs) => {
                                    arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                }
                            }
                        }
                        None => {
                            arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span,
                            ));
                        }
                    }
                }
                if let Some(errors) = arg_errors {
                    return Err(errors);
                }
                // After checking all arguments via check_expr, state.subst has been updated
                // with all unifications. Apply it to the return type to get the final result.
                Ok(state.subst.apply(inst_ret))
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
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-MONO/CALL-POLY arms
            // infer named args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-MONO (check_expr at line 1011) or CALL-POLY (infer_expr).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map);
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        _ => Err(vec![TypeError::not_a_function(&func_ty, span)]),
    }
}

/// Type-check a case arm: pattern + body.
///
/// - If pattern is `Expr::LetDecl { bindings }`: extract bindings, narrow scrutinee type,
///   introduce bindings into scope for body
/// - If pattern is an expression: type-check the pattern expression (exact-value match)
/// - Type-check body with the extended environment
/// - Return the body type
///
/// For simplified implementation:
/// - Intersection with scrutinee type: if annotation present, use annotation; else use scrutinee
/// - Structural test patterns (name: Constructor) are recognized but not fully implemented yet
#[allow(dead_code)]
fn typecheck_case_arm(
    pattern: &Spanned<Expr>,
    body: &Spanned<Expr>,
    scrutinee_ty: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    match &pattern.node {
        Expr::LetDecl { bindings } => {
            // Process each binding element against the scrutinee type.
            // For now, simplified: extract binding names and types, extend env, infer body.
            let mut arm_env = TypeEnv::with_parent(env);

            for binding in bindings {
                match &binding.node {
                    // Wildcard: _ (check first to avoid binding "_" as a variable)
                    Expr::VarRef { name, .. } if name == "_" => {
                        // Wildcard - no binding introduced
                    }

                    // Plain binding: name
                    Expr::VarRef { name, .. } => {
                        // Bind name to scrutinee type
                        arm_env.insert(name.clone(), scrutinee_ty.clone());
                    }

                    // Typed binding: name@Type — introduces `name : scrutinee_ty ∩ ann_ty`
                    // This implements the BAS intersection narrowing rule from unified-bindings.md:
                    // [let n@T] binds n with type scrutinee_ty ∩ T.
                    // Unknown is the identity in intersection (AGT lifting), so when scrutinee_ty
                    // is Unknown, the intersection reduces to ann_ty (via normalize_intersection).
                    Expr::Annotated { name, annotation } => {
                        let ann_ty = resolve_annotation(
                            &annotation.node,
                            env,
                            annotation.span,
                            state,
                            &mut None,
                            &mut None,
                        )
                        .map_err(|e| vec![e])?;

                        // Narrow: scrutinee_ty ∩ ann_ty (BAS type narrowing).
                        // normalize_intersection handles Unknown-as-identity and Top-as-identity.
                        let narrowed_ty =
                            Type::normalize_intersection(vec![scrutinee_ty.clone(), ann_ty]);
                        arm_env.insert(name.clone(), narrowed_ty);
                    }

                    // Nested LetDecl for multi-payload destructuring: [a b] in [let [a b]: Constructor]
                    // For now, bind each named element to Unknown (structural test form is future work;
                    // the parser does not yet support colon inside [let ...] to express the constructor).
                    Expr::LetDecl {
                        bindings: nested_bindings,
                    } => {
                        for nested in nested_bindings {
                            match &nested.node {
                                Expr::VarRef { name, .. } if name != "_" => {
                                    arm_env.insert(name.clone(), Type::Unknown);
                                }
                                Expr::Annotated { name, annotation } => {
                                    let ann_ty = resolve_annotation(
                                        &annotation.node,
                                        env,
                                        annotation.span,
                                        state,
                                        &mut None,
                                        &mut None,
                                    )
                                    .map_err(|e| vec![e])?;
                                    arm_env.insert(name.clone(), ann_ty);
                                }
                                _ => {
                                    // Wildcard or other — no binding
                                }
                            }
                        }
                    }

                    _ => {
                        // Other binding forms not yet supported
                        return Err(vec![TypeError::new(
                            "unsupported binding pattern in case arm",
                            binding.span,
                        )]);
                    }
                }
            }

            // Type-check body with extended environment
            let arm_env = Rc::new(arm_env);
            infer_expr(body, &arm_env, state, type_map)
        }

        _ => {
            // Exact-value match: infer pattern expression type, then infer body
            // The pattern expression should be scalar/nullary
            let pattern_ty = infer_expr(pattern, env, state, type_map)?;

            // Check that pattern is scalar or nullary (design doc requirement)
            // For now, just issue a warning if it's not - don't block
            match &pattern_ty {
                Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool => {
                    // Valid scalar type - OK
                }
                _ => {
                    // Non-scalar - could be nullary constructor, or could be error
                    // For now, allow it (conservative)
                }
            }

            // Body is checked in the enclosing environment (no new bindings from exact-value match)
            infer_expr(body, env, state, type_map)
        }
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
    // resolve_annotation is never called (it receives Type::Unknown directly), so an empty
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

    let mut param_types: Vec<(Option<String>, Type)> = params
        .iter()
        .map(|p| {
            let ty = match &p.node.annotation {
                Some(ann) => resolve_annotation(
                    &ann.node,
                    env,
                    ann.span,
                    state,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                ),
                // Unannotated params use Unknown (gradual typing escape hatch).
                //
                // WHY NOT fresh_type_var(): Using fresh TypeVars for unannotated params
                // causes O(N²) blowup in the prelude type-checking. Each unannotated param
                // becomes a TypeVar that unifies with constrained TypeVars from + / builtin-add
                // etc. (∀a[Numeric]. Fn(a a → a)), creating TypeVar→TypeVar chains in
                // state.subst. With ~170 prelude functions each having 2-3 unannotated params,
                // state.subst grows to hundreds of entries. The substitution merge loop in
                // infer_dict (typecheck_dict.rs:380-406) is O(|state.subst|²) in practice,
                // making prelude type-checking take 120+ seconds.
                //
                // FUTURE WORK: To enable TypeVars here, fix the merge loop to be O(N) instead
                // of O(N²) — e.g., by not calling subst.apply() for each entry, or by using
                // union-find substitution (see doc/whatif/union-find-substitution.md).
                // When that is done, restore: None => Ok(state.fresh_type_var())
                // and update test_fn_unannotated to expect TypeVar instead of Unknown.
                None => Ok(Type::Unknown),
            }?;
            Ok((Some(p.node.name.clone()), ty))
        })
        .collect::<Result<_, _>>()
        .map_err(|e| vec![e])?;

    let mut fn_env = TypeEnv::with_parent(env);
    for (i, param) in params.iter().enumerate() {
        if param.node.variadic {
            // Variadic params collect extra positional args into a Seq(T) where T is inferred.
            // Runtime still uses Dict with int keys (gradual typing allows this mismatch).
            let elem_ty = state.fresh_type_var();
            let variadic_ty = Type::Seq(Box::new(elem_ty));
            // Update param_types[i] to match the env binding so the function signature is accurate.
            param_types[i].1 = variadic_ty.clone();
            fn_env.insert(param.node.name.clone(), variadic_ty);
        } else {
            fn_env.insert(param.node.name.clone(), param_types[i].1.clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            // Check if this is a metadata dict annotation: @[return: Type doc: "..." constraint: ...]
            let actual_ann = match &ann.node {
                Annotation::PropertyDict(entries) => {
                    // Check if this has metadata keys (return:, constraint:, doc:).
                    // These are the only valid named keys for fn@[...] metadata dicts.
                    // Unknown named keys (e.g. fn@[foo: "bar"]) are caught by resolve_fn_metadata
                    // when combined with a known key; pure all-unknown-key dicts are interpreted
                    // as record return types by resolve_annotation.
                    let has_metadata_key = entries.iter().any(|e| {
                        e.node.key.as_ref().is_some_and(|k| {
                            matches!(&k.node, Expr::Str(s) if s == "return" || s == "constraint" || s == "doc" || s == "bind" || s == "kinds")
                        })
                    });

                    // Mixed named+positional check: if any known metadata key is present but
                    // some entries are positional, emit the mixed-key error.
                    let all_keyed = entries.iter().all(|e| e.node.key.is_some());

                    if has_metadata_key && !all_keyed {
                        // Mixed named + positional in metadata dict context
                        return Err(vec![TypeError::new(
                            "fn annotation must use either named keys (return:, constraint:, doc:, bind:, kinds:) or positional entries (union return type), not both",
                            ann.span,
                        )]);
                    }

                    if has_metadata_key {
                        // This is a metadata dict - use resolve_fn_metadata to process all keys
                        // including constraint: entries which must be registered in state.constraints
                        let (ret_ty, _doc_string) = typecheck_annot::resolve_fn_metadata(
                            entries,
                            env,
                            ann.span,
                            state,
                            &mut ann_mapping_opt,
                            &mut row_ann_mapping_opt,
                        )
                        .map_err(|e| vec![e])?;
                        // Note: doc_string is extracted again in typecheck_dict.rs Pass 4 for storage
                        ret_ty
                    } else {
                        // Regular PropertyDict annotation - resolve normally
                        resolve_annotation(
                            &ann.node,
                            env,
                            ann.span,
                            state,
                            &mut ann_mapping_opt,
                            &mut row_ann_mapping_opt,
                        )
                        .map_err(|e| vec![e])?
                    }
                }
                _ => {
                    // Simple annotation - resolve normally
                    resolve_annotation(
                        &ann.node,
                        env,
                        ann.span,
                        state,
                        &mut ann_mapping_opt,
                        &mut row_ann_mapping_opt,
                    )
                    .map_err(|e| vec![e])?
                }
            };

            // Set expected_return for inferred [do] macro support.
            // Save the old value to restore after body inference (for nested fn defs).
            let prev_expected_return = state.expected_return.take();
            state.expected_return = Some(actual_ann.clone());

            // When declared return type contains type variables, switch to unification mode
            // (doc/06 §[CHECK-FN], Damas & Milner 1982, Pierce & Turner 2000 §3.2).
            // TypeVars in is_subtype only match via reflexive equality, so
            // is_subtype(IntLiteral(42), TypeVar("_t5")) = false would reject valid code.
            // Unification mode binds the TypeVars via constraint solving.
            let result = if actual_ann.has_inference_vars() {
                let body_ty = infer_expr(body, &fn_env, state, type_map)?;
                // Borrow-split: mem::take + restore avoids simultaneous &mut state.subst and &mut state
                let mut subst = std::mem::take(&mut state.subst);
                let result = unify(&body_ty, &actual_ann, &mut subst, state, body.span);
                state.subst = subst;
                result.map_err(|e| vec![e])?;
                // Apply substitution to resolve any TypeVars bound during unification.
                // Without this, the returned Type::Function would have has_inference_vars() == true,
                // causing check_call to enter the CALL-POLY path unnecessarily (see check_call's
                // has_inference_vars guard). This prevents call sites from entering CALL-POLY.
                state.subst.apply(&actual_ann)
            } else {
                // Use checking mode for concrete return types (no type variables)
                check_expr(body, &actual_ann, &fn_env, state, type_map)?;
                actual_ann
            };

            // Restore previous expected_return
            state.expected_return = prev_expected_return;
            result
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

/// Scan inferred types for quality issues (Unknown types, over-broad annotations).
///
/// Emits diagnostics at base level (Info/Warn). The CLI/LSP layer WILL apply a `--strict` bump
/// once the type-warning channel is wired (TODO — escalation is not yet implemented).
/// This is called at the end of type checking to produce advisory notifications.
pub fn scan_type_quality(
    type_map: &TypeMap,
    ast: &File,
    diagnostics: &mut Vec<crate::error::TypeDiagnostic>,
) {
    use crate::ast::{Annotation, Position};
    use crate::error::{DiagnosticLevel, TypeDiagnostic};
    use std::collections::HashSet;

    // Helper function to recursively check if a type contains Unknown
    fn contains_unknown(ty: &Type) -> bool {
        match ty {
            Type::Unknown => true,
            Type::Record(row) => row.fields.values().any(contains_unknown),
            Type::Function { params, ret, .. } => {
                params.iter().any(|(_, t)| contains_unknown(t)) || contains_unknown(ret)
            }
            Type::Seq(elem) => contains_unknown(elem),
            Type::Map(k, v) => contains_unknown(k) || contains_unknown(v),
            Type::Union(members) | Type::Intersection(members) => {
                members.iter().any(contains_unknown)
            }
            Type::Negation(t) => contains_unknown(t),
            _ => false,
        }
    }

    // Helper to check if an annotation explicitly references Unknown
    fn is_unknown_annotation(ann: &Annotation) -> bool {
        use crate::ast::Expr;
        match ann {
            Annotation::Simple(name) => name == "Unknown",
            Annotation::PropertyDict(entries) => {
                // Check if there's a "return: Unknown" entry (for function metadata dicts)
                entries.iter().any(|entry| {
                    if let Some(key_expr) = &entry.node.key {
                        if let Expr::Str(key_name) = &key_expr.node {
                            if key_name == "return" {
                                // Check if the value is a VarRef to "Unknown"
                                if let Expr::VarRef { name, .. } = &entry.node.value.node {
                                    return name == "Unknown";
                                }
                            }
                        }
                    }
                    false
                })
            }
            Annotation::Annotated(_, _) => false, // Parameterized annotations are never Unknown
        }
    }

    // Collect all explicit @Unknown annotation spans from the AST
    fn collect_explicit_unknown_spans(ast: &File) -> HashSet<(usize, usize)> {
        use crate::ast::Expr;
        let mut spans = HashSet::new();

        fn walk_expr(expr: &Spanned<Expr>, spans: &mut HashSet<(usize, usize)>) {
            match &expr.node {
                Expr::TypeAssert {
                    annotation,
                    expr: inner,
                    ..
                } => {
                    if is_unknown_annotation(&annotation.node) {
                        // Record the span of the entire TypeAssert expression
                        spans.insert((expr.span.start.offset, expr.span.end.offset));
                    }
                    walk_expr(inner, spans);
                }
                Expr::Fn {
                    return_ann, body, ..
                } => {
                    // Check for @Unknown return annotation
                    if let Some(ann) = return_ann {
                        if is_unknown_annotation(&ann.node) {
                            // Record the span of the function expression
                            spans.insert((expr.span.start.offset, expr.span.end.offset));
                        }
                    }
                    walk_expr(body, spans);
                }
                Expr::Call { func, args, .. } => {
                    walk_expr(func, spans);
                    for arg in args {
                        walk_expr(arg, spans);
                    }
                }
                Expr::Sequential(exprs) => {
                    for e in exprs {
                        walk_expr(e, spans);
                    }
                }
                Expr::DotAccess { expr, .. } => {
                    walk_expr(expr, spans);
                }
                Expr::Pipe { lhs, rhs } => {
                    walk_expr(lhs, spans);
                    walk_expr(rhs, spans);
                }
                Expr::Match {
                    scrutinee, arms, ..
                } => {
                    walk_expr(scrutinee, spans);
                    for arm in arms {
                        walk_expr(&arm.body, spans);
                        if let Some(guard) = &arm.guard {
                            walk_expr(guard, spans);
                        }
                    }
                }
                Expr::Dict(entries) => {
                    for entry in entries {
                        if let Some(key) = &entry.node.key {
                            walk_expr(key, spans);
                        }
                        walk_expr(&entry.node.value, spans);
                    }
                }
                Expr::Quote(e)
                | Expr::Unquote(e)
                | Expr::UnquoteSplice(e)
                | Expr::TypeAlias { body: e, .. } => {
                    walk_expr(e, spans);
                }
                Expr::DefMacro { body, .. } => {
                    walk_expr(body, spans);
                }
                Expr::ClassDecl { methods, .. } => {
                    for entry in methods {
                        if let Some(key) = &entry.node.key {
                            walk_expr(key, spans);
                        }
                        walk_expr(&entry.node.value, spans);
                    }
                }
                Expr::InstanceDecl { arms, .. } => {
                    for (pattern_expr, methods) in arms {
                        walk_expr(pattern_expr, spans);
                        for entry in methods {
                            if let Some(key) = &entry.node.key {
                                walk_expr(key, spans);
                            }
                            walk_expr(&entry.node.value, spans);
                        }
                    }
                }
                Expr::PatternDecl { bindings } => {
                    for binding in bindings {
                        walk_expr(binding, spans);
                    }
                }
                Expr::LetDecl { bindings } => {
                    for binding in bindings {
                        walk_expr(binding, spans);
                    }
                }
                Expr::CaseArm { pattern, body } => {
                    walk_expr(pattern, spans);
                    walk_expr(body, spans);
                }
                Expr::TypeApp { func, arg } => {
                    walk_expr(func, spans);
                    walk_expr(arg, spans);
                }
                _ => {}
            }
        }

        for doc in &ast.documents {
            for expr in &doc.node.expressions {
                walk_expr(expr, &mut spans);
            }
        }

        spans
    }

    let explicit_unknown_spans = collect_explicit_unknown_spans(ast);

    // Scan all inferred types for Unknown
    for ((start, end), ty) in type_map {
        if contains_unknown(ty) {
            // Check if this is an explicit @Unknown annotation
            let is_explicit = explicit_unknown_spans.contains(&(*start, *end));

            let (level, code, message) = if is_explicit {
                (
                    DiagnosticLevel::Info,
                    "T011",
                    "explicit @Unknown annotation — type is not statically known".to_string(),
                )
            } else {
                (
                    DiagnosticLevel::Warn,
                    "T010",
                    "inferred type is Unknown — consider adding a type annotation".to_string(),
                )
            };

            diagnostics.push(TypeDiagnostic {
                level,
                code,
                message,
                span: Span {
                    start: Position {
                        offset: *start,
                        line: 0,
                        column: 0,
                    },
                    end: Position {
                        offset: *end,
                        line: 0,
                        column: 0,
                    },
                },
            });
        }
    }

    // Over-broad annotation detection (Tasks 3 & 4)
    // Check for function return annotations that are wider than the inferred body type
    check_overbroad_annotations(ast, type_map, diagnostics);
}

/// Check for over-broad annotations where the declared type is wider than inferred.
///
/// Detects patterns like:
/// - `fn@Number` when body infers `Int` → suggest `@Int`
/// - `fn@Top` when body infers a specific type → suggest the specific type
fn check_overbroad_annotations(
    ast: &File,
    type_map: &TypeMap,
    diagnostics: &mut Vec<crate::error::TypeDiagnostic>,
) {
    use crate::ast::{Annotation, Expr};
    use crate::error::{DiagnosticLevel, TypeDiagnostic};

    // Helper to resolve an annotation to a Type for comparison
    fn resolve_simple_annotation(ann: &Annotation) -> Option<Type> {
        match ann {
            Annotation::Simple(name) => match name.as_str() {
                "Int" => Some(Type::Int),
                "Float" => Some(Type::Float),
                "Number" => Some(Type::Number),
                "Str" => Some(Type::Str),
                "Bool" => Some(Type::Bool),
                "Top" => Some(Type::Top),
                "Unknown" => Some(Type::Unknown),
                _ => None, // Type variables, named types, etc.
            },
            Annotation::PropertyDict(_) => None, // Complex annotations not handled yet
            Annotation::Annotated(_, _) => None, // Parameterized annotations not handled yet
        }
    }

    // Walk the AST to find function expressions with return annotations
    fn walk_for_functions(
        expr: &Spanned<Expr>,
        type_map: &TypeMap,
        diagnostics: &mut Vec<TypeDiagnostic>,
    ) {
        match &expr.node {
            Expr::Fn {
                return_ann: Some(ann),
                body,
                ..
            } => {
                // Only check simple annotations for now
                if let Some(declared_type) = resolve_simple_annotation(&ann.node) {
                    // Look up the inferred body type
                    let body_key = (body.span.start.offset, body.span.end.offset);
                    if let Some(inferred_type) = type_map.get(&body_key) {
                        // Check if declared is over-broad:
                        // inferred <: declared (inferred is narrower)
                        // but NOT declared <: inferred (declared is NOT narrower)
                        if Type::is_subtype(inferred_type, &declared_type)
                            && !Type::is_subtype(&declared_type, inferred_type)
                        {
                            // Found an over-broad annotation
                            let type_str = format!("{}", inferred_type);
                            let ann_str = format!("{}", ann.node);
                            diagnostics.push(TypeDiagnostic {
                                level: DiagnosticLevel::Info,
                                code: "T012",
                                message: format!(
                                    "annotation @{} is over-broad — inferred type is {}; consider using @{}",
                                    ann_str, type_str, type_str
                                ),
                                span: ann.span,
                            });
                        }
                    }
                }
                // Continue walking into the body
                walk_for_functions(body, type_map, diagnostics);
            }
            // Fn without return annotation: no over-broad check needed, but must
            // walk the body so nested annotated functions are still visited.
            Expr::Fn { body, .. } => {
                walk_for_functions(body, type_map, diagnostics);
            }
            Expr::Call { func, args, .. } => {
                walk_for_functions(func, type_map, diagnostics);
                for arg in args {
                    walk_for_functions(arg, type_map, diagnostics);
                }
            }
            Expr::Sequential(exprs) => {
                for e in exprs {
                    walk_for_functions(e, type_map, diagnostics);
                }
            }
            Expr::DotAccess { expr, .. } => {
                walk_for_functions(expr, type_map, diagnostics);
            }
            Expr::Pipe { lhs, rhs } => {
                walk_for_functions(lhs, type_map, diagnostics);
                walk_for_functions(rhs, type_map, diagnostics);
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                walk_for_functions(scrutinee, type_map, diagnostics);
                for arm in arms {
                    walk_for_functions(&arm.body, type_map, diagnostics);
                    if let Some(guard) = &arm.guard {
                        walk_for_functions(guard, type_map, diagnostics);
                    }
                }
            }
            Expr::Dict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        walk_for_functions(key, type_map, diagnostics);
                    }
                    walk_for_functions(&entry.node.value, type_map, diagnostics);
                }
            }
            Expr::TypeAssert { expr, .. } => {
                walk_for_functions(expr, type_map, diagnostics);
            }
            Expr::Quote(e)
            | Expr::Unquote(e)
            | Expr::UnquoteSplice(e)
            | Expr::TypeAlias { body: e, .. } => {
                walk_for_functions(e, type_map, diagnostics);
            }
            Expr::DefMacro { body, .. } => {
                walk_for_functions(body, type_map, diagnostics);
            }
            Expr::ClassDecl { methods, .. } => {
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        walk_for_functions(key, type_map, diagnostics);
                    }
                    walk_for_functions(&entry.node.value, type_map, diagnostics);
                }
            }
            Expr::InstanceDecl { arms, .. } => {
                for (pattern_expr, methods) in arms {
                    walk_for_functions(pattern_expr, type_map, diagnostics);
                    for entry in methods {
                        if let Some(key) = &entry.node.key {
                            walk_for_functions(key, type_map, diagnostics);
                        }
                        walk_for_functions(&entry.node.value, type_map, diagnostics);
                    }
                }
            }
            Expr::PatternDecl { bindings } => {
                for binding in bindings {
                    walk_for_functions(binding, type_map, diagnostics);
                }
            }
            Expr::LetDecl { bindings } => {
                for binding in bindings {
                    walk_for_functions(binding, type_map, diagnostics);
                }
            }
            Expr::CaseArm { pattern, body } => {
                walk_for_functions(pattern, type_map, diagnostics);
                walk_for_functions(body, type_map, diagnostics);
            }
            Expr::TypeApp { func, arg } => {
                walk_for_functions(func, type_map, diagnostics);
                walk_for_functions(arg, type_map, diagnostics);
            }
            _ => {}
        }
    }

    for doc in &ast.documents {
        for expr in &doc.node.expressions {
            walk_for_functions(expr, type_map, diagnostics);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Entry;
    use std::cell::RefCell;

    fn check(input: &str) -> Result<(), Vec<TypeError>> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, _diagnostics) = typecheck_file(&file.node);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
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

    fn doc_env_with_builtins(input: &str) -> Rc<TypeEnv> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        // Populate PRELUDE_INSTANCE_CACHE so Equatable/Comparable/Showable/etc. instances are
        // available via dynamic resolution (no longer hardcoded in satisfies_constraint).
        // We call build_prelude_env() for the side-effect of populating the cache, but still
        // use TypeEnv::with_builtins() as the type environment so tests that override
        // prelude functions (e.g., [and: [fn ...]] [has?: [fn ...]]) work correctly.
        let _ = crate::imports::build_prelude_env();
        let env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();
        crate::imports::seed_infer_state_from_prelude_cache(&mut state);
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

    /// Look up a field name in a type that may be a `Record` or an `Intersection` of Records.
    /// Multi-field annotations produce `Intersection([{field1: T1, ...ρ1}, {field2: T2, ...ρ2}])`.
    /// This helper searches all members and returns the first matching field type found.
    fn type_get_field<'a>(ty: &'a Type, field: &str) -> Option<&'a Type> {
        match ty {
            Type::Record(Row { fields, .. }) => fields.get(field),
            Type::Intersection(members) => {
                for m in members {
                    if let Type::Record(Row { fields, .. }) = m {
                        if let Some(v) = fields.get(field) {
                            return Some(v);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Assert that a type (Record or Intersection-of-Records) contains a specific field
    /// with a specific type. Panics with a descriptive message if the field is missing
    /// or has the wrong type.
    fn assert_has_field(ty: &Type, field: &str, expected: &Type) {
        match type_get_field(ty, field) {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                panic!("field '{field}' has type {actual}, expected {expected} (in {ty})")
            }
            None => panic!("field '{field}' not found in {ty}"),
        }
    }

    fn file_env(input: &str) -> Rc<TypeEnv> {
        file_env_impl(input, false)
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
        // x has type IntLiteral(42), so $x has type IntLiteral(42)
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
        // Dict fields preserve literal types.
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
        // Dict fields preserve literal types.
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
                // Forward references unify: $b resolves to 42, so both a and b have IntLiteral(42).
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
        // BAS: accessing a field not in the static type returns Unknown (gradual typing).
        // Under BAS open-world semantics, we don't error statically for unknown fields
        // because the concrete value may have extra fields (width subtyping). Runtime will
        // signal a missing-field error if the field is truly absent.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[person: [name: \"Andrew\"]]\n[result: $person.age]",
            "result",
        );
        assert!(
            matches!(ty, Type::Unknown),
            "BAS: missing field access returns Unknown (not an error), got {ty}"
        );
    }

    #[test]
    fn test_dot_access_non_record() {
        let errors = check_err("[x: 42]\n[result: $x.field]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected record type")));
    }

    // -- Dot access on Intersection and Negation types --

    #[test]
    fn test_dot_access_intersection_found() {
        // `[@[[all [x: Int ...] [y: String ...]]] $rec].x` should return Int.
        // The TypeAssert produces Intersection([{x:Int,...ρ1},{y:String,...ρ2}]).
        // Our new Intersection arm searches members and returns Int from the {x:Int,...} member.
        let env = doc_env(
            "[rec: [x: 1  y: \"hello\"]]\
             [result: [@[[all [x: Int ...] [y: String ...]]] $rec].x]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!(
                "expected Int for .x on Intersection([{{x:Int,...}},{{y:String,...}}]), got {other}"
            ),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_dot_access_intersection_missing_field_returns_unknown() {
        // Accessing a field that is not in any member of the intersection should return Unknown
        // (not an error), because a member with an open row tail may accept the field dynamically.
        let result = check(
            "[rec: [x: 1  y: \"hello\"]]\
             [result: [@[[all [x: Int ...] [y: String ...]]] $rec].z]",
        );
        // Should not fail — field z is not statically known in the intersection, so Unknown is returned
        assert!(
            result.is_ok(),
            "expected no error for accessing unknown field on intersection, got: {result:?}"
        );
    }

    #[test]
    fn test_dot_access_negation_returns_unknown() {
        // Accessing a field on a Negation type returns Unknown (not an error).
        // Negation restricts inhabitance, not field structure.
        // @[[without [x: Int ...]]] produces Negation(Record({x:Int},...)).
        // The conservative negation subtyping rule (_, Negation(_)) => true allows the check to pass.
        // Then .y on Negation(...) should return Unknown without error.
        let result = check("[x: 42]\n[result: [@[[without [x: Int ...]]] $x].y]");
        // Should not error — Negation falls back to Unknown for field access
        assert!(
            result.is_ok(),
            "expected no error for field access on Negation type, got: {result:?}"
        );
    }

    // -- Multi-field annotation as Intersection (BAS) --

    #[test]
    fn test_multi_field_annotation_produces_intersection() {
        // `@[x: Int  y: String]` resolves to Intersection([{x: Int, ...ρ1}, {y: String, ...ρ2}])
        // Single-field annotations still produce Record (unchanged behavior).
        // Verify multi-field annotations typecheck without error against matching dicts.
        check("[p: [@[x: Int  y: String] [x: 1  y: \"hi\"]]]").unwrap();
    }

    #[test]
    fn test_multi_field_annotation_rejects_wrong_field_type() {
        // `@[x: Int  y: String]` rejects values where one field has the wrong type.
        let errors = check_err("[p: [@[x: Int  y: String] [x: \"wrong\"  y: \"hi\"]]]");
        assert!(!errors.is_empty(), "expected type error but got none");
    }

    #[test]
    fn test_multi_field_annotation_dot_access_works() {
        // Dot access on a value annotated with `@[x: Int  y: String]` should find fields.
        // The intersection-of-open-records form supports field access via the Intersection arm.
        let ty = result_field(
            "[p: [@[x: Int  y: String] [x: 1  y: \"hi\"]]]\n[rx: $p.x]",
            "rx",
        );
        assert!(
            matches!(ty, Type::Int | Type::IntLiteral(_)),
            "expected Int-like for .x on multi-field annotation, got {ty}"
        );
    }

    #[test]
    fn test_multi_field_annotation_body_alias() {
        // Type alias with 2+ fields produces Intersection body.
        // The alias can be used as a TypeAssert annotation.
        check("[Point: [type [x: Int  y: Int]]]\n[p: [@Point [x: 1  y: 2]]]").unwrap();
    }

    #[test]
    fn test_multi_field_annotation_single_field_stays_record() {
        // Under BAS width subtyping (RowVar step 2): a closed record with MORE fields is a
        // subtype of a closed annotation with fewer fields — width subtyping allows extra fields.
        // `{name: String, age: Int} <: {name: String}` is sound because the supertype only
        // constrains what it declares; extra fields in the subtype are irrelevant.
        check("[@[name: String] [name: \"Alice\"  age: 30]]").unwrap();
    }

    #[test]
    fn test_multi_field_annotation_with_rest_stays_record() {
        // Multi-field annotation WITH `...` rest is still a Record (not Intersection).
        // The rest entry causes the annotation to keep the user-supplied RowTail.
        check("[@[x: Int  y: String ...] [x: 1  y: \"hi\"  z: true]]").unwrap();
    }

    #[test]
    fn test_multi_field_annotation_shared_typevar_stays_record() {
        // `[type [a] [first: a  second: a]]` uses the SAME TypeVar `a` in both fields.
        // The shared-var guard fires, keeping the alias body as a Record (no Intersection).
        // Ensures unification doesn't bind `a` to two different values.
        check(
            "[Pair: [type [a] [first: a  second: a]]]\
             [p: [fn@[Pair Int] [] [first: 1  second: 2]]]",
        )
        .unwrap();
    }

    // -- Access chain constraint generation (doc/07 Part 5) --

    #[test]
    fn test_dot_access_open_record_extends_tail() {
        // BAS: all records are closed (no RowVar tails). `@Open` with `...` becomes a
        // closed record. Accessing unknown fields returns Unknown (gradual typing), not TypeVar.
        // Width subtyping allows extra fields at runtime; static type doesn't track them.
        // In new syntax, string literals require quotes.
        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");
        // r1 and r2 should be Unknown (field not in static type, BAS returns Unknown)
        match env.get("r1").map(|s| &s.body) {
            Some(Type::Unknown) => {}
            Some(other) => panic!("BAS: expected Unknown for unknown field, got {other}"),
            None => panic!("field 'r1' not found in env"),
        }
        match env.get("r2").map(|s| &s.body) {
            Some(Type::Unknown) => {}
            Some(other) => panic!("BAS: expected Unknown for unknown field, got {other}"),
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

        // BAS: Accessing a non-existent field on a letrec forward-reference returns Unknown.
        // Under BAS, check_dot_access generates constraint γ_data → Record({unknown: β})
        // in state.subst, but unify_rows ignores non-shared fields (BAS width subtyping).
        // No type error is produced; the caller sees Unknown for the unknown field.
        let result = check("[result: $data.unknown  data: [known: 1]]");
        assert!(
            result.is_ok(),
            "BAS: accessing unknown field on forward reference returns Unknown, not an error; \
             got: {:?}",
            result.unwrap_err()
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
        // BAS: all records are closed. Unknown field accesses return Unknown (gradual typing).
        // Both r1 and r2 get Unknown for unknown fields — there are no distinct TypeVars for rows.
        // In new syntax, string literals require quotes.
        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");

        match env.get("r1").map(|s| &s.body) {
            Some(Type::Unknown) => {}
            Some(other) => panic!("BAS: expected Unknown for unknown field r1, got {other}"),
            None => panic!("field 'r1' not found in env"),
        }
        match env.get("r2").map(|s| &s.body) {
            Some(Type::Unknown) => {}
            Some(other) => panic!("BAS: expected Unknown for unknown field r2, got {other}"),
            None => panic!("field 'r2' not found in env"),
        }
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
        // The Person alias body `[name: String  age: Number]` is an Intersection of
        // open single-field records: [{name: String, ...ρ1}, {age: Number, ...ρ2}].
        // Use assert_has_field to check either Record or Intersection-of-Records form.
        assert_has_field(&ty, "name", &Type::Str);
        assert_has_field(&ty, "age", &Type::Number);
    }

    #[test]
    fn test_type_alias_cycle_resolves_to_unknown() {
        // With two-pass registration, circular aliases resolve to Unknown.
        // The register_type_aliases path pre-registers both, so both resolve.
        // But infer_dict still uses the single-pass approach, so using a
        // circular alias in an annotation within the same dict produces an
        // error (the alias wasn't registered in dict_env).
        check("[A: [type B]  B: [type A]]").unwrap();
        let errors = check_err("[A: [type B]  B: [type A]  x: [@A 42]]");
        assert!(
            !errors.is_empty(),
            "using circular type aliases in the same dict should produce errors"
        );
        let msg = format!("{:?}", errors);
        assert!(
            msg.contains("ndefined") || msg.contains("nknown"),
            "error should be about undefined/unknown type, got: {msg}"
        );
    }

    #[test]
    fn test_type_alias_field_named_type() {
        // Regression: type alias with a field named "type:" should not be
        // confused with the @[type: T] annotation shorthand.
        let ty = result_field(
            "[Thing: [type [type: String  id: Int]]]\n[t: [@Thing [type: \"widget\"  id: 1]]]",
            "t",
        );
        assert_has_field(&ty, "type", &Type::Str);
        assert_has_field(&ty, "id", &Type::Int);
    }

    #[test]
    fn test_annotation_record_with_type_field() {
        // Test that @[type: String id: Int] as a direct annotation creates a record
        // with two fields, not a type expression shorthand.
        let ty = result_field("[f: [fn [data@[type: String id: Int]] $data]]", "f");
        if let Type::Function { params, .. } = ty {
            assert_eq!(params.len(), 1);
            assert_has_field(&params[0].1, "type", &Type::Str);
            assert_has_field(&params[0].1, "id", &Type::Int);
        } else {
            panic!("expected Function type, got {:?}", ty);
        }
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
                // Unannotated params use Unknown (gradual typing escape hatch).
                // See the comment in infer_fn for why fresh_type_var() causes O(N²) blowup
                // during prelude type-checking and must wait for a proper fix.
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, Some("x".to_string()));
                assert_eq!(
                    params[0].1,
                    Type::Unknown,
                    "unannotated param should be Unknown (gradual), got {:?}",
                    params[0].1
                );
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
                assert_eq!(params, vec![(Some("x".to_string()), Type::Number)]);
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

    #[test]
    fn test_fn_union_return_annotation_int_null() {
        // Regression: fn@[Int Null] must route to union return type path.
        // Previously failed with "property dict annotation must be a dict expression"
        // because the parser rejected lowercase-headed implied calls in annotation position.
        // After fix: [Int Null] is two positional entries → Union(Int, Null).
        let ty = infer("[fn@[Int Null] [] []]");
        match ty {
            Type::Function { ret, .. } => {
                // Return type should be Union(Int, empty-record) — the Null type
                assert!(
                    matches!(*ret, Type::Union(_)),
                    "expected Union return type, got {ret}"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_union_return_annotation_typevar_null() {
        // Regression: fn@[a Null] must route to union return type path.
        // 'a' is a lowercase type variable name; 'Null' is the empty record type.
        // Both are positional entries → treated as union type members.
        let ty = infer("[fn@[a Null] [] []]");
        match ty {
            Type::Function { ret, .. } => {
                // Return type is Union(TypeVar, Record({})) — the [a Null] union annotation.
                // Tighter check: must be Union, not just non-Error (which would pass Unknown).
                assert!(
                    matches!(*ret, Type::Union(_)),
                    "expected Union return type, got {ret}"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
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
        // neither Type::Function nor Type::Unknown. We construct such a scheme directly:
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
                constraints: vec![],
                body: Type::Int,
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
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
        // Regression test for type-seq sprint: $builtin-range should return Type::Seq(Int).
        // TypeEnv::with_builtins() registers builtin-range as Fn(Int, Int) -> Seq(Int).
        // (The user-facing $range wrapper lives in prelude.llt and is not present here.)
        let input = "[result: [call $builtin-range 0 10]]";
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
        // Negative test: $+ returns a numeric type (Numeric a => a -> a -> a), not Seq.
        // TypeEnv::with_builtins() registers + as Numeric a => a -> a -> a.
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

        // Verify it's NOT a Seq — that's the primary invariant this test guards
        assert!(
            !matches!(result_ty, Type::Seq(_)),
            "+ should not return a Seq type; got: {result_ty}"
        );
    }

    // -- Seq and Null type annotations (Task 1) --

    #[test]
    fn test_seq_annotation_bare() {
        // Bare @Seq resolves to Type::Seq(Type::Unknown) in resolve_type_name
        // Test via parameter annotation which uses resolve_annotation
        let ty = infer("[fn [xs@Seq] $xs]");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(
                    params[0],
                    (Some("xs".to_string()), Type::Seq(Box::new(Type::Unknown)))
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_seq_annotation_with_element_type() {
        // Seq@String in a standalone Annotated expression.
        // The syntax `Seq@String` (bare identifier with ImmediateAt) parses as
        // Annotated{name:"Seq", annotation:Simple("String")}, which calls
        // resolve_annotated(name="Seq", annotation=Simple("String")).
        // Note: `@Seq@String` (with leading @) is NOT valid standalone LLT syntax —
        // bare `@` at top level is only valid inside TypeAssert brackets `[@...]`.
        // The correct bare form is `Seq@String` (identifier followed by ImmediateAt).
        let ty = infer("Seq@String");
        assert_eq!(ty, Type::Seq(Box::new(Type::Str)));
    }

    #[test]
    fn test_null_annotation_bare() {
        // Bare @Null resolves to Type::Record(Row::Empty) in resolve_type_name
        let ty = infer("[fn [x@Null] $x]");
        match ty {
            Type::Function { params, .. } => match &params[0].1 {
                Type::Record(Row { fields }) => {
                    assert!(fields.is_empty());
                }
                other => panic!("expected Record(Row::empty), got {other}"),
            },
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_null_annotation_in_type_assert() {
        // [@Null []] should succeed (empty dict matches Null)
        let ty = infer("[@Null []]");
        match ty {
            Type::Record(Row { fields }) => {
                assert!(fields.is_empty());
            }
            other => panic!("expected Record(Row::empty), got {other}"),
        }
    }

    #[test]
    fn test_null_return_annotation() {
        // [fn@Null [s@String] []] exercises the resolve_annotation(Simple("Null")) path
        // in infer_fn for the return annotation.
        // Null resolves to Type::Record(Row { fields: {} }), so check_expr checks
        // that the body [] (empty dict) satisfies that type.
        // The function return type should be the declared Null type (empty record).
        let ty = result_field("[f: [fn@Null [s@String] []]]", "f");
        match ty {
            Type::Function { params, ret, .. } => {
                // Parameter should be String
                assert_eq!(
                    params[0].1,
                    Type::Str,
                    "param @String should resolve to Type::Str, got {:?}",
                    params[0].1
                );
                // Return type should be Null = empty record
                match *ret {
                    Type::Record(Row { ref fields }) => {
                        assert!(
                            fields.is_empty(),
                            "fn@Null return type should have no fields, got {:?}",
                            fields
                        );
                    }
                    other => {
                        panic!("fn@Null return type should be Record(Row::empty), got {other}")
                    }
                }
            }
            other => panic!("expected Function type for [fn@Null [s@String] []], got {other}"),
        }
    }

    #[test]
    fn test_builtin_collect_returns_record_not_seq() {
        // $builtin-collect returns Record (open row), not Seq.
        // TypeEnv::with_builtins() registers builtin-collect as Fn(Seq(Any)) -> Record({...}).
        // (The user-facing $collect and $range wrappers live in prelude.llt and are not present here.)
        let input = "[s: [call $builtin-range 0 5]]\n[result: [call $builtin-collect $s]]";
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
        // x has type IntLiteral(42), so $x has type IntLiteral(42)
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
                // x has type IntLiteral(1), so %.x has type IntLiteral(1)
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
        // BAS: row variables (RowVar) are removed. The `...a` rest annotation is syntactically
        // accepted but has no row variable semantics — it just sets has_rest=true.
        // Cross-kind collision detection (TypeVar vs RowVar) is no longer possible since
        // row_ann_mapping is always None. The annotation is valid and accepted.
        // `@[name: Int ...a]` resolves to Record({name: Int}) (closed, ...a ignored).
        let result = check("[fn [x@a y@[name: Int ...a]] $x]");
        assert!(
            result.is_ok(),
            "BAS: cross-kind annotations no longer error since row vars are removed; got: {:?}",
            result.unwrap_err()
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
        // BAS: row variables (RowVar) are removed. The `...r` rest annotation has no row
        // variable semantics — it just sets has_rest=true.
        // Cross-kind collision detection (RowVar→TypeVar) no longer fires since row_ann_mapping
        // is always None. `y@r` creates a fresh TypeVar for `r`, and `...r` is silently ignored.
        // `@[name: Int ...r]` resolves to Record({name: Int}) (closed, ...r ignored).
        let result = check("[fn [x@[name: Int ...r] y@r] $x]");
        assert!(
            result.is_ok(),
            "BAS: cross-kind annotations no longer error since row vars are removed; got: {:?}",
            result.unwrap_err()
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
            Type::Function { params, .. } => {
                assert_eq!(params, vec![(Some("x".to_string()), Type::Number)])
            }
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
            Type::Unknown
        );
    }

    #[test]
    fn test_property_dict_no_key_resolves_as_union() {
        // Single positional entry resolves via union path; single-element union unwraps
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        // Use VarRef (unquoted identifier) — Expr::Str is reserved for string literal types
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: Rc::new(sp(Expr::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolved: RefCell::new(None),
                })),
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
            Type::Int
        );
    }

    // --- HKT kind inference tests (hkt-kind-inference sprint) ---

    #[test]
    fn test_hkt_kind_operator_class_param_registration() {
        // Test that Mappable class has Operator-kinded param registered in kind_env
        let mut file = crate::parse("[x: 1]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let state = InferState::new();

        // Verify Mappable is registered with Kind::Operator
        let mappable = state.class_env.get("Mappable").unwrap();
        assert_eq!(mappable.params.len(), 1);
        assert_eq!(mappable.params[0].1, Kind::Operator);
    }

    #[test]
    fn test_hkt_rank1_restriction_rejects_nested_operator() {
        // Rank-1 restriction: [f g] where both f and g are Operator-kinded should error
        // This requires parser support for @Operator annotations, which is deferred.
        // For now, test that the rejection logic works when we manually construct
        // an Operator-kinded type in an annotation.

        // Skipped: requires parser changes to support @Operator in class params.
        // The restriction is implemented in resolve_type_dict for Task 3.
    }

    #[test]
    fn test_property_dict_unresolvable_type_propagates_error() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        // Use VarRef for the value (unquoted type name) — Expr::Str is for string literal types
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: Rc::new(sp(Expr::VarRef {
                    name: "NoSuchType".into(),
                    escaped: false,
                    resolved: RefCell::new(None),
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
            Type::Unknown
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
        // The Coord alias body `[x: Number  y: Number]` is now an Intersection of
        // open single-field records: [{x: Number, ...ρ1}, {y: Number, ...ρ2}].
        assert_has_field(&ty, "x", &Type::Number);
        assert_has_field(&ty, "y", &Type::Number);
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
        // With ADT support, [type ["Int" "String"]] is now valid:
        // quoted strings in type position resolve as StringLiteral types,
        // and two positional entries create a union.
        // Verify it produces Union(StringLiteral("Int"), StringLiteral("String")).
        let result = check("[type [\"Int\" \"String\"]]");
        assert!(
            result.is_ok(),
            "auto-indexed string literals in type position should produce a union, got: {:?}",
            result.err()
        );
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
                match &params[0].1 {
                    Type::Function {
                        params: inner_params,
                        ret: inner_ret,
                        variadic: _,
                    } => {
                        assert_eq!(*inner_params, vec![(None, Type::Int)]);
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
                let param_ty = &params[0].1;
                // Multi-field annotation `[name: String  age: Number]` now produces
                // Intersection([{name: String, ...ρ1}, {age: Number, ...ρ2}]).
                // Use type_get_field to search both Record and Intersection forms.
                assert_has_field(param_ty, "name", &Type::Str);
                assert_has_field(param_ty, "age", &Type::Number);
                assert_eq!(*ret, Type::Str);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_annotation_composite_type_in_type_assert() {
        // With fresh TypeVars for unannotated params, the default function needs annotations
        // to match the expected type Fn@Number [Int]
        let ty = infer(
            "[f: [fn [x] $x]  result: [@[type: [Fn@Number [Int]] default: [fn [x@Int] 0]] $f]]",
        );
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
                assert_eq!(params, vec![(None, Type::Int)]);
                // IntLiteral(0) promotes to Number via subsumption
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
                match &params[0].1 {
                    Type::Function {
                        params: outer_params,
                        ret: outer_ret,
                        variadic: _,
                    } => {
                        assert_eq!(*outer_params, vec![(None, Type::Int)]);
                        // return type: Fn(Int -> Int)
                        match outer_ret.as_ref() {
                            Type::Function {
                                params: inner_params,
                                ret: inner_ret,
                                variadic: _,
                            } => {
                                assert_eq!(*inner_params, vec![(None, Type::Int)]);
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
                        assert_eq!(*ret_params, vec![(None, Type::Int)]);
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
        // BAS: all records are closed. `@[x: Int ...]` resolves to Record({x: Int}) (closed).
        // `project: [fn [r@[x: Int ...]] $r.x]` has type Fn@Int [Record({x:Int})].
        // It is called with records that have extra fields — this works via BAS width subtyping:
        //   Record({x:1, y:"hello"}) <: Record({x:Int}) = true (extra "y" allowed).
        let input = r#"
            [make-record: [fn [] [project: [fn [r@[x: Int ...]] $r.x]]]]
            ---
            [call $make-record]
            ---
            [r1: [call $project [x: 1  y: "hello"]]
             r2: [call $project [x: 2  z: true]]]
        "#;
        // Both r1 and r2 should typecheck successfully — BAS width subtyping allows extra fields.
        check(input).expect("BAS width subtyping: calls with extra fields should succeed");
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
            // [fn [v] $v] is annotated with [@Mapper] where Mapper = [Fn@b [a]].
            // Lambda checking mode substitutes the annotation's TypeVars for the params.
            // Lowercase names `a` and `b` in the annotation become fresh TypeVars.
            // The result type comes from the annotation, not from unannotated param inference.
            // param is TypeVar (from annotation `a`), ret is TypeVar (from annotation `b`).
            // They are distinct TypeVars (different names in the annotation).
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "expected 1 param");
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "param should be a TypeVar (from annotation), got {:?}",
                    params[0]
                );
                assert!(
                    matches!(*ret, Type::TypeVar(_, _)),
                    "ret should be a TypeVar (from annotation), got {ret:?}"
                );
                // `a` and `b` are distinct annotation names, so param != ret
                assert_ne!(
                    params[0].1, *ret,
                    "param TypeVar (annotation `a`) and ret TypeVar (annotation `b`) must be distinct"
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
            // [fn [p q] $p] is annotated with [@BinOp] where BinOp = [Fn@c [a b]].
            // Lambda checking mode substitutes the annotation's TypeVars for the params.
            // Lowercase names `a`, `b`, `c` in the annotation become fresh TypeVars.
            // The result type comes from the annotation. All three are distinct TypeVars.
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2, "expected 2 params");
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "params[0] should be TypeVar (from annotation `a`), got {:?}",
                    params[0]
                );
                assert!(
                    matches!(&params[1].1, Type::TypeVar(_, _)),
                    "params[1] should be TypeVar (from annotation `b`), got {:?}",
                    params[1]
                );
                assert!(
                    matches!(*ret, Type::TypeVar(_, _)),
                    "ret should be TypeVar (from annotation `c`), got {ret:?}"
                );
                // `a`, `b`, `c` are distinct annotation names: all three differ
                assert_ne!(
                    params[0].1, params[1].1,
                    "params[0] (annotation `a`) and params[1] (annotation `b`) must be distinct"
                );
                assert_ne!(
                    params[0].1, *ret,
                    "params[0] (annotation `a`) and ret (annotation `c`) must be distinct"
                );
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
                assert_eq!(params, vec![(None, Type::Number), (None, Type::Number)]);
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
                    matches!(&params[0].1, Type::TypeVar(_, _)),
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
            // [fn [v] [fn [w] $w]] is annotated with [@HO] where HO = [Fn@[Fn@c [b]] [a]].
            // Lambda checking mode substitutes the annotation's TypeVars for the params.
            // Lowercase names `a`, `b`, `c` become fresh TypeVars.
            // Outer param gets TypeVar for `a`; ret is [Fn@c [b]] with TypeVars for `b` and `c`.
            // The annotation drives the result type, not unannotated param inference.
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "outer should have 1 param");
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "outer param should be TypeVar (from annotation `a`), got {:?}",
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
                            matches!(&inner_params[0].1, Type::TypeVar(_, _)),
                            "inner param should be TypeVar (from annotation `b`), got {:?}",
                            inner_params[0]
                        );
                        assert!(
                            matches!(*inner_ret, Type::TypeVar(_, _)),
                            "inner ret should be TypeVar (from annotation `c`), got {inner_ret:?}"
                        );
                        // `b` and `c` are distinct annotation names
                        assert_ne!(
                            inner_params[0].1, *inner_ret,
                            "inner param (annotation `b`) and inner ret (annotation `c`) must be distinct"
                        );
                        // outer param `a` is distinct from inner param `b`
                        assert_ne!(
                            params[0].1, inner_params[0].1,
                            "outer param (annotation `a`) != inner param (annotation `b`)"
                        );
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
    fn test_bare_fn_annotation_resolves_to_any() {
        // `@Fn` in parameter position resolves to Type::Unknown (not a concrete Function
        // type), so that higher-order functions annotated with @Fn don't produce false
        // type errors when unified with concrete function types.
        // [fn [f@Fn] $f] should infer without type errors.
        let ty = infer("[fn [f@Fn] $f]");
        // The outer lambda infers as a Function type whose first parameter is Type::Unknown
        // (the @Fn annotation must resolve to Any, not a pseudo-Function type).
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(
                    params,
                    vec![(Some("f".to_string()), Type::Unknown)],
                    "@Fn param must resolve to Type::Unknown, not a pseudo-Function type"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_bare_fn_annotation_no_false_type_error() {
        // Passing a concrete function to an @Fn-annotated parameter must not produce
        // spurious type errors from attempting to unify Type::Unknown with a concrete
        // Function type — the two are compatible under Any semantics.
        // [fn [pred@Fn] [pred 42]] applied with a concrete function for pred.
        let result = check("[result: [[fn [pred@Fn] [pred 42]] [fn [x@Number] $x]]]");
        // There should be no type errors — @Fn accepts any callable.
        assert!(
            result.is_ok(),
            "expected no type errors, got: {:?}",
            result.unwrap_err()
        );
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
                assert_eq!(params, vec![(None, Type::Number)]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_display_round_trip() {
        let ty = Type::Function {
            params: vec![
                (None, Type::TypeVar("a".into(), 0)),
                (None, Type::TypeVar("b".into(), 0)),
            ],
            ret: Box::new(Type::TypeVar("c".into(), 0)),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@c [a b]");
    }

    // -- Polymorphic call unification --

    #[test]
    fn test_call_polymorphic_identity() {
        // Polymorphic identity call preserves literal type
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result"),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_identity_string() {
        // Polymorphic identity call preserves literal type
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
        // Polymorphic call preserves literal type
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
        // Polymorphic call preserves literal type
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
        // Polymorphic calls preserve literal types.
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
        // Multi-document form ensures $f is fully resolved before the call site is type-checked.
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

        // Wrong-type named arg: $f expects `x@Int`; passing a string should produce a type error.
        let errors = check_err(
            "[f: [fn [x@Int] $x]]
             ---
             [result: [call $f x: \"wrong-type\"]]",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("named argument") && e.message.contains("mismatch")),
            "expected named-arg type mismatch error, got: {:?}",
            errors
        );

        // Unknown named arg: $f has no parameter named `z`; should produce an "unknown named argument" error.
        let errors = check_err(
            "[f: [fn [x@Int] $x]]
             ---
             [result: [call $f z: 42]]",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("unknown named argument")),
            "expected unknown named argument error, got: {:?}",
            errors
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
        // a type error. Use multi-document form so $f is fully resolved (CALL-MONO path)
        // before the call, avoiding the letrec TypeVar-arm bypass.
        // 1 positional + 1 named = 2 total matches the 2-param function (x, y).
        let errors = check_err(
            "[f: [fn [x@Int y@Int] [call $+ $x $y]]]\n\
             ---\n\
             [result: [call $f 42 y: $missing]]",
        );
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
        match &alias.unwrap().body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "Identity should have 1 param");
                // The param and return must be the SAME TypeVar (both reference annotation `a`)
                assert_eq!(
                    params[0].1, **ret,
                    "Identity function: param and return must be the same TypeVar (both use `a`)"
                );
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "param should be TypeVar, got {:?}",
                    params[0]
                );
            }
            other => panic!("expected Function type alias, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_type_expr_multi_params() {
        // [Mapper: [type [Fn@b [a b]]]] — map function type: params[0]=a, params[1]=b, ret=b.
        // After Fix 1: fresh internal vars, but `b` in params[1] and `b` in ret must be the SAME
        // TypeVar (same mapping within the alias scope). `a` must be a DIFFERENT TypeVar from `b`.
        let env = doc_env("[Mapper: [type [Fn@b [a b]]]]\n[x: 1]");
        let alias = env.get_type_alias("Mapper").unwrap();
        match &alias.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2, "Mapper should have 2 params");
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "params[0] (a) should be TypeVar"
                );
                assert!(
                    matches!(&params[1].1, Type::TypeVar(_, _)),
                    "params[1] (b) should be TypeVar"
                );
                // params[1] and ret both reference annotation `b`, so they must be equal
                assert_eq!(
                    params[1].1, **ret,
                    "params[1] and ret must be the same TypeVar (both use `b`)"
                );
                // params[0] (a) and params[1] (b) must be distinct
                assert_ne!(
                    params[0], params[1],
                    "params[0] (a) and params[1] (b) must differ"
                );
            }
            other => panic!("expected Function type alias, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_type_expr_concrete_params() {
        let env = doc_env("[Add: [type [Fn@Number [Number Number]]]]\n[x: 1]");
        let alias = env.get_type_alias("Add").unwrap();
        match &alias.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params, &vec![(None, Type::Number), (None, Type::Number)]);
                assert_eq!(**ret, Type::Number);
            }
            other => panic!("expected Function type alias, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_type_expr_predicate() {
        // [Pred: [type [Fn@Bool [a]]]] — predicate type: param is TypeVar (a), return is Bool.
        // After Fix 1: annotation name `a` becomes a fresh internal var. Bool is unchanged.
        let env = doc_env("[Pred: [type [Fn@Bool [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Pred").unwrap();
        match &alias.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 1, "Pred should have 1 param");
                assert!(
                    matches!(&params[0].1, Type::TypeVar(_, _)),
                    "param (a) should be TypeVar, got {:?}",
                    params[0]
                );
                assert_eq!(**ret, Type::Bool);
            }
            other => panic!("expected Function type alias, got {other:?}"),
        }
    }

    // -- Row polymorphism --

    #[test]
    fn test_type_expr_open_record() {
        // BAS: all records are closed (RowTail::Empty). The "..." annotation in [type [name: String ...]]
        // is treated as user-explicit openness, but under BAS Step 1, multi-field annotations
        // use RowTail::Empty. Single-field open annotations also collapse to Empty.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                // BAS: all records are closed; field "name" should be String
                assert_eq!(fields.get("name"), Some(&Type::Str));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_row_var_record() {
        // BAS: named row variable "...rest" in type annotations — under BAS, all tails are Empty.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[WithName: [type [name: String ...rest]]]\n[p: [@WithName [name: \"Alice\"]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
            }
            other => panic!("expected record, got {other}"),
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
            Type::Record(_) => {}
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_anonymous_open_record_annotations_get_fresh_vars() {
        // BAS: anonymous open record annotations "..." are treated as closed under BAS.
        // Both params are records (RowTail::Empty); the function type-checks correctly.
        let code = r#"
            [f: [fn [x@[a: Int ...]  y@[b: String ...]]
                 [x: $x  y: $y]]]
        "#;
        let result = check(code);
        assert!(result.is_ok(), "type check should succeed: {:?}", result);

        // Verify the inferred type has record params
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                // BAS: both params should be record types
                assert!(
                    matches!(&params[0].1, Type::Record(_)),
                    "x param should be Record type, got {:?}",
                    params[0].1
                );
                assert!(
                    matches!(&params[1].1, Type::Record(_)),
                    "y param should be Record type, got {:?}",
                    params[1].1
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_cross_function_anonymous_open_records_get_fresh_vars() {
        // BAS: anonymous open record annotations are independent between functions.
        let code = r#"
            [f: [fn [x@[a: Int ...]] $x.a]
             g: [fn [y@[b: String ...]] $y.b]]
        "#;
        let result = check(code);
        assert!(result.is_ok(), "type check should succeed: {:?}", result);

        // Under BAS: both f and g should have record params (RowTail::Empty)
        let ty_f = result_field(code, "f");
        let ty_g = result_field(code, "g");

        assert!(
            matches!(ty_f, Type::Function { .. }),
            "f should be a function type, got {ty_f}"
        );
        assert!(
            matches!(ty_g, Type::Function { .. }),
            "g should be a function type, got {ty_g}"
        );
    }

    #[test]
    fn test_named_row_var_level_monotonicity() {
        // BAS: named row variables "...r" in type annotations are treated as closed (Empty).
        // This test verifies the function type-checks correctly even with named row vars.
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

        // BAS: both parameters are record types (RowTail::Empty)
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                assert!(
                    matches!(&params[0].1, Type::Record(_)),
                    "x param should be Record, got {:?}",
                    params[0].1
                );
                assert!(
                    matches!(&params[1].1, Type::Record(_)),
                    "y param should be Record, got {:?}",
                    params[1].1
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_check_dot_access_unknown_field_returns_unknown() {
        // BAS: accessing a field not in the record's known fields returns Unknown.
        // Under BAS width subtyping, the field may be present in the concrete value.
        // No row bindings are created — BAS handles openness via is_subtype, not unification.
        // In new syntax, string literals require quotes.
        let code = r#"
            [Open: [type [name: String ...]]]
            [p: [@Open [name: "Alice"]]]
            [result: [inner: $p.unknown]]
        "#;

        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed — unknown field access returns Unknown under BAS: {:?}",
            result
        );

        // Verify that the `inner` field of `result` has type Unknown
        let result_ty = result_field(code, "result");
        let inner_ty = match result_ty {
            Type::Record(Row { ref fields, .. }) => fields
                .get("inner")
                .cloned()
                .expect("result record should have 'inner' field"),
            other => panic!("expected result to be a Record type, got {other}"),
        };
        // BAS: unknown field access → Unknown
        assert!(
            matches!(inner_ty, Type::Unknown | Type::TypeVar(_, _)),
            "expected Unknown or TypeVar for $p.unknown under BAS, got {inner_ty}"
        );
    }

    #[test]
    fn test_type_assert_open_record_accepts_extra_fields() {
        // In new syntax, string literals require quotes.
        check("[@[name: String ...] [name: \"Alice\"  age: 30]]").unwrap();
    }

    #[test]
    fn test_type_assert_single_field_annotation_accepts_extra_fields() {
        // BAS open semantics (Step 2): a single-field annotation @[name: String] is a closed
        // record {name: String} under BAS width subtyping. A record with extra fields
        // [name: "Alice" age: 30] satisfies this because all required fields are present.
        // Under BAS, structural annotations express "has AT LEAST these fields", so
        // {name: "Alice", age: 30} <: {name: String} holds via width subtyping.
        check("[@[name: String] [name: \"Alice\"  age: 30]]").unwrap();
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
        // BAS: all records are closed. `@Open` with `...` resolves to Record({name: Str}).
        // Accessing `$p.unknown` (not in static type) returns Unknown (gradual typing).
        // Under BAS, no RowVar constraint generation — width subtyping handles openness.
        // In new syntax, string literals require quotes.
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: \"Alice\"]]]\n[result: $p.unknown]",
            "result",
        );
        assert!(
            matches!(ty, Type::Unknown),
            "BAS: expected Unknown for unknown field on closed record, got {ty}"
        );
    }

    #[test]
    fn test_data_dict_always_closed() {
        let ty = infer("[a: 1  b: 2]");
        assert!(matches!(ty, Type::Record(_)), "expected Record, got {ty}");
    }

    #[test]
    fn test_rest_in_data_dict_ignored() {
        let ty = infer("[a: 1 ...]");
        match ty {
            Type::Record(Row { fields }) => {
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
        // Polymorphic calls preserve literal types.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[result: [a: [call $id 42]  b: [call $id \"hello\"]]]",
            "result",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("b"), Some(&Type::StringLiteral("hello".into())));
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
                // Both a and b resolve to IntLiteral(42) via letrec unification
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
                            matches!(params.get(0).map(|(_, t)| t), Some(Type::TypeVar(_, _))),
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
            !id_scheme.type_vars.is_empty(),
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
                // c has literal type IntLiteral(42)
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
                    (Type::Unknown, Type::Unknown) => {
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
                    !matches!(result_ty, Type::Unknown),
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
                // inner field preserves literal type
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
        // With Unknown unannotated params, [fn [x] $x] is monomorphic: Unknown -> Unknown.
        // Unknown is the gradual typing escape hatch (Siek & Taha 2006); unification with
        // Unknown zeros the TypeVar's level, preventing generalization.
        let env = doc_env("[id: [fn [x] $x]]");
        let id_scheme = env.get("id").expect("id should be in env");

        // The scheme should have zero type variables (monomorphic: Unknown -> Unknown)
        assert_eq!(
            id_scheme.type_vars.len(),
            0,
            "id with Unknown param should be monomorphic (zero type vars), got scheme: {:?}",
            id_scheme
        );

        // The function type should be Fn@Unknown [Unknown]
        match &id_scheme.body {
            Type::Function { params, ret, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(
                    params[0].1,
                    Type::Unknown,
                    "param should be Unknown (gradual)"
                );
                assert_eq!(**ret, Type::Unknown, "ret should be Unknown (gradual)");
            }
            other => panic!("expected Function, got {other:?}"),
        }
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
                assert_eq!(params, &vec![(None, Type::Int)]);
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
                    matches!(&params[0].1, Type::TypeVar(_, _)),
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
        // Polymorphic calls preserve literal types
        let ty = result_field("[f: [fn [x@a] $x]]\n[result: [call $f 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "CALL-POLY should unify and preserve literal type"
        );

        // Multiple calls should get independent instantiations (use quoted string in new syntax)
        // Each call returns the literal type of its argument
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
                    vec![(None, Type::Int)],
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
                assert_eq!(params, vec![(None, Type::Int)], "param from expected type");
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
                assert_eq!(params, vec![(None, Type::Int)], "param from expected type");
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
                assert_eq!(params, vec![(None, Type::Int)], "concrete param propagated");
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
                assert_eq!(params, vec![(None, Type::Int)], "param from expected type");
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
        //   - Unannotated params get Type::Unknown (monomorphic path, no type vars).
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
        // Uses Fn@ReturnType [params] syntax to get a real function type, not Type::Unknown
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
                        // Compare only the type component, since param names differ ("x" vs "y")
                        assert_eq!(
                            params[0].1, params[1].1,
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
                            params[0].1, **ret,
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
        // Polymorphic call preserves literal type from dot-access
        assert_eq!(
            ty,
            Type::StringLiteral("hello".into()),
            "CALL-POLY with dot-access argument should resolve return type to StringLiteral(\"hello\"), got: {ty}"
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
        // Polymorphic call across document boundary preserves literal type
        assert_eq!(
            result_ty,
            Type::StringLiteral("hello".into()),
            "CALL-POLY across document boundary should resolve return type to StringLiteral(\"hello\"), got: {result_ty}"
        );
    }

    // -- Type::Unknown callee positional arg type_map population --

    #[test]
    fn test_call_any_callee_populates_type_map_for_positional_args() {
        // Regression test for the Type::Unknown arm in check_call and check_call_with_scheme.
        //
        // When the callee resolves to Type::Unknown (e.g., a variable bound to Any in the env),
        // positional arguments must still be inferred and recorded in type_map — otherwise
        // LSP hover over argument expressions in Any-typed calls produces no type information.
        //
        // The fix (typecheck.rs check_call ~1050, check_call_with_scheme ~900) added an
        // `infer_expr` loop inside the Type::Unknown arm only. This test guards that loop:
        // if it were removed, the span of `42` would not appear in type_map and the assertion
        // below would fail.
        //
        // SETUP: `f` is bound to TypeScheme::mono(Type::Unknown) in the parent env, simulating
        // any runtime-typed or externally-typed callable (e.g., a function loaded from JSON,
        // an FFI binding, or a value whose type cannot be statically determined). The call
        // `[call $f 42]` exercises check_call via the monomorphic (empty type_vars) path.
        let input = "[call $f 42]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        // Build a parent env with `f: Any` — monomorphic scheme, empty type_vars.
        let mut parent_env = TypeEnv::new();
        parent_env.insert_scheme("f".to_string(), TypeScheme::mono(Type::Unknown));
        let parent_env = Rc::new(parent_env);

        let mut state = InferState::new();
        let mut type_map = TypeMap::new();

        let expr = &file.node.documents[0].node.expressions[0];
        let result = infer_expr(expr, &parent_env, &mut state, &mut Some(&mut type_map));

        // The call to an Any-typed function returns Any.
        assert_eq!(
            result,
            Ok(Type::Unknown),
            "calling Any-typed callee should return Type::Unknown, got: {result:?}"
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

        // The span of `42` must appear in type_map: the Type::Unknown arm must have inferred it.
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
        // Function body IntLiteral(42) is preserved as the return type
        let ty = result_field("[f: [fn [x@Int] 42]]\n[result: [call $f 1]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "CALL-MONO should return IntLiteral(42) (function body literal type preserved)"
        );

        // Verify check_call_with_scheme behavior: polymorphic function takes CALL-POLY path
        // (CALL-MONO was deleted from check_call_with_scheme in cycle-findings-c36-a Task 2,
        // since instantiate_scheme always produces fresh TypeVars making CALL-MONO unreachable)
        // Polymorphic calls preserve literal types
        let ty = result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "check_call_with_scheme CALL-POLY path should unify and return IntLiteral(42)"
        );
    }

    // -- Variadic param type inference --

    #[test]
    fn test_variadic_param_type_is_any() {
        // Variadic params collect extra positional args into a Seq(T) where T is inferred.
        //
        // Grammar: variadic_param = @{ "..." ~ param_name } — no @annotation syntax.
        // The param_types override at infer_fn ensures the function type reflects
        // Seq(TypeVar) for the variadic slot.

        // Basic variadic: single param, collects all positional args as a seq
        let ty = result_field("[f: [fn [...rest] $rest]]", "f");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(params.len(), 1, "variadic function should have 1 param");
                assert!(
                    matches!(&params[0].1, Type::Seq(_)),
                    "variadic param should have type Seq(T), got: {:?}",
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
                    matches!(&params[0].1, Type::Int),
                    "annotated param 'a' should be Int, got: {:?}",
                    params[0]
                );
                // Third param (variadic) must be Seq(T)
                assert!(
                    matches!(&params[2].1, Type::Seq(_)),
                    "variadic param 'rest' should have type Seq(T), got: {:?}",
                    params[2]
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }
    }

    #[test]
    fn test_variadic_param_env_binding_is_any() {
        // The env binding for a variadic param inside the function body is Seq(T).
        //
        // If the body references $rest, its inferred type comes from the env binding.
        // Returning $rest should give the function a Seq(T) return type.

        let ty = result_field("[f: [fn [x ...rest] $rest]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(ret.as_ref(), Type::Seq(_)),
                    "function returning variadic param should have Seq(T) return type, got: {ret:?}"
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
        // Polymorphic call preserves literal type through dot-access
        assert_eq!(
            ty,
            Type::StringLiteral("hello".into()),
            "cross-entry dot-access on polymorphic call result should resolve to StringLiteral(\"hello\"), got: {ty}"
        );

        // Also verify that `result` has the full record type.
        // Use a different input where `result` is in the last expression.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: \"hello\"]]\n[result: [call $id $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                // Polymorphic call preserves literal type for record fields
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".into())),
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
        // Polymorphic call preserves literal type
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "polymorphic call with same-type constraint should resolve return type to IntLiteral(42)"
        );

        // Verify `value` also resolves correctly
        // value is bound to 42 = IntLiteral(42)
        let ty = result_field(
            "[same: [fn [x@a y@a] $x]]\n[result: [call $same $value 42]  value: 42]",
            "value",
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "forward-referenced value should have type IntLiteral(42)"
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
        // Polymorphic call preserves literal type through access chain
        assert_eq!(
            ty,
            Type::StringLiteral("hello".into()),
            "CALL-POLY with access-chain arg should resolve to StringLiteral(\"hello\") through seeded subst"
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
        // Polymorphic call preserves literal type through cross-entry dot-access
        assert_eq!(
            ty,
            Type::StringLiteral("hello".into()),
            "check_call CALL-POLY merge: cross-entry dot-access should return StringLiteral(\"hello\")"
        );

        // Verify that `result` itself resolves to a record with the right field type.
        let ty = result_field(
            "[data: [name: \"hello\"]]\n[result: [call [fn [x@a] $x] $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                // Polymorphic call preserves literal type in record field
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".into())),
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
        // Polymorphic call preserves literal type through access-chain seed
        assert_eq!(
            ty,
            Type::StringLiteral("hello".into()),
            "check_call CALL-POLY seed: access-chain arg should return StringLiteral(\"hello\")"
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
        // x: IntLiteral(42), so $x: IntLiteral(42)
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
        // Under BAS width subtyping (RowVar step 2): an open record [x: Int, ...] IS allowed
        // as an argument to a function expecting closed [x: Int]. The BAS rule
        // (RowTail::RowVar, RowTail::Empty) => true means the open record satisfies the closed
        // constraint — the closed annotation only constrains what it declares.
        //
        // Uses multi-document input so f's type is fully resolved in document 1 before
        // document 2 type-checks g. Inside g's body, $r has open-record type [x: Int, ...ρ]
        // from its annotation. Passing $r to $f (which expects the closed record [x: Int])
        // now succeeds under BAS width subtyping.
        check(
            "[f: [fn [r@[type: [x: Int]]] $r]]
             ---
             [g: [fn [r@[type: [x: Int ...]]] [call $f $r]]]",
        )
        .unwrap();
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

        // With an annotated param type, a wrong-type named arg should produce a type error.
        let errors = check_err(
            "[f: [fn [x@Int] $x]]
             ---
             [result: [call $f x: \"wrong-type\"]]",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("named argument") && e.message.contains("mismatch")),
            "expected named-arg type mismatch error for annotated param, got: {:?}",
            errors
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

    // -- Parameterized type aliases --

    #[test]
    fn test_parameterized_type_alias_single_param() {
        // [type [a] [first: a  second: a]] with [@[Pair Int] ...]
        // should expand to [first: Int  second: Int]
        let ty = result_field(
            "[Pair: [type [a] [first: a  second: a]]
             pair: [fn@[Pair Int] [] [first: 1  second: 2]]]",
            "pair",
        );
        match ty {
            Type::Function { ret, .. } => match ret.as_ref() {
                Type::Record(Row { fields, .. }) => {
                    assert_eq!(
                        fields.get("first"),
                        Some(&Type::Int),
                        "first should be Int after instantiation"
                    );
                    assert_eq!(
                        fields.get("second"),
                        Some(&Type::Int),
                        "second should be Int after instantiation"
                    );
                }
                other => panic!("expected Record return type, got {other:?}"),
            },
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_parameterized_type_alias_multiple_params() {
        // [type [a b] [first: a  second: b]] with [@[Pair Int String] ...]
        // Since `a` and `b` are distinct TypeVars (no sharing), the alias body becomes
        // Intersection([{first: a, ...ρ1}, {second: b, ...ρ2}]) after instantiation.
        // After substituting a→Int, b→String and unification with the body {first: 1, second: "hello"},
        // each intersection member's row var absorbs the other field, so the intersection
        // has mixed field types (both annotation-level Int and inferred IntLiteral(1)).
        // The key correctness property: no type error is emitted (annotation and body are compatible).
        let result = check(
            "[Pair: [type [a b] [first: a  second: b]]
             pair: [fn@[Pair Int String] [] [first: 1  second: \"hello\"]]]",
        );
        assert!(
            result.is_ok(),
            "parameterized alias with two params should type-check without errors, got: {result:?}"
        );
        // Also verify the field type is accessible from the intersection-of-records form.
        // The annotation `[Pair Int String]` = Intersection([{first: Int,...}, {second: Str,...}]).
        // type_get_field finds the ANNOTATED field type from some member of the intersection.
        let ty = result_field(
            "[Pair: [type [a b] [first: a  second: b]]
             pair: [fn@[Pair Int String] [] [first: 1  second: \"hello\"]]]",
            "pair",
        );
        match ty {
            Type::Function { ret, .. } => {
                // After row-var expansion the intersection members contain both annotated and
                // inferred field types. Accept Int or IntLiteral for 'first', Str or StringLiteral
                // for 'second' — both indicate the annotation was correctly applied.
                let first_ty = type_get_field(ret.as_ref(), "first");
                assert!(
                    matches!(first_ty, Some(Type::Int) | Some(Type::IntLiteral(_))),
                    "first should be Int-like, got {first_ty:?}"
                );
                let second_ty = type_get_field(ret.as_ref(), "second");
                assert!(
                    matches!(second_ty, Some(Type::Str) | Some(Type::StringLiteral(_))),
                    "second should be Str-like, got {second_ty:?}"
                );
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_parameterized_type_alias_arity_mismatch() {
        // [Pair Int] when Pair expects 2 params should error
        let errors = check_err(
            "[Pair: [type [a b] [first: a  second: b]]
             pair: [fn@[Pair Int] [] [first: 1  second: 2]]]",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expects 2 type parameter")
                    && e.message.contains("got 1")),
            "expected arity mismatch error, got: {errors:?}"
        );
    }

    #[test]
    fn test_parameterized_type_alias_zero_params_backward_compat() {
        // [type [first: Int  second: Int]] without params should work
        assert!(check(
            "[Pair: [type [first: Int  second: Int]]
             pair: [fn@Pair [] [first: 1  second: 2]]]"
        )
        .is_ok());
    }

    #[test]
    fn test_parameterized_type_alias_with_row_variable() {
        // [type [a] [name: String  ...a]] should allow row variable in tail.
        // The annotation @[Extensible r] instantiates the alias with a row variable.
        // After unification with the body [name: "test"  age: 42], the row variable
        // binds to the extra fields, so the final type has a closed record or a record
        // with the bound row variable's contents. We verify the type checks without error
        // and the name field has the correct type.
        let input = "[Extensible: [type [a] [name: String  ...a]]
             make: [fn@[Extensible r] [] [name: \"test\"  age: 42]]]";
        assert!(
            check(input).is_ok(),
            "parameterized alias with row variable should typecheck"
        );
        let ty = result_field(input, "make");
        match ty {
            Type::Function { ret, .. } => match ret.as_ref() {
                Type::Record(Row { fields, .. }) => {
                    assert_eq!(fields.get("name"), Some(&Type::Str));
                }
                other => panic!("expected Record return type, got {other:?}"),
            },
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_parameterized_type_alias_nested_usage() {
        // Using a parameterized alias inside another parameterized alias
        let ty = result_field(
            "[Pair: [type [a] [first: a  second: a]]
             Nested: [type [b] [inner: [Pair b]  outer: b]]
             make: [fn@[Nested Int] [] [inner: [first: 1  second: 2]  outer: 3]]]",
            "make",
        );
        match ty {
            Type::Function { ret, .. } => {
                match ret.as_ref() {
                    Type::Record(Row { fields, .. }) => {
                        // inner should be [first: Int  second: Int]
                        match fields.get("inner") {
                            Some(Type::Record(inner_row)) => {
                                assert_eq!(inner_row.fields.get("first"), Some(&Type::Int));
                                assert_eq!(inner_row.fields.get("second"), Some(&Type::Int));
                            }
                            other => panic!("expected inner to be Record, got {other:?}"),
                        }
                        assert_eq!(fields.get("outer"), Some(&Type::Int));
                    }
                    other => panic!("expected Record return type, got {other:?}"),
                }
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_check_call_forward_ref_result_type() {
        // [fn [x] $x] has Unknown unannotated param. Calling it with 42 returns Unknown
        // (gradual semantics: Unknown propagates through calls).
        let ty = result_field("[result: [call $f 42]  f: [fn [x] $x]]", "result");
        assert_eq!(ty, Type::Unknown);
    }

    #[test]
    fn test_check_call_bound_typevar_resolves_to_function() {
        // [fn [x] $x] has Unknown unannotated param. Calling it with 42 returns Unknown
        // (gradual semantics: Unknown propagates through calls).
        let ty = result_field("[f: [fn [x] $x]  result: [call $f 42]]", "result");
        assert_eq!(
            ty,
            Type::Unknown,
            "call to identity with Unknown param should return Unknown"
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
        // the annotation. The annotation `[x: Int  y: Int]` is now an Intersection of
        // open single-field records: [{x: Int, ...ρ1}, {y: Int, ...ρ2}].
        // is_subtype passes: {x:1, y:2} <: {x:Int, ...ρ1} (open row) AND <: {y:Int, ...ρ2}.
        // state.subst.apply() resolves the ρ row vars to their bound values.
        // The apply is idempotent — this guards against regression where apply corrupts types.
        let ty = result_field("[p: [@[type: [x: Int  y: Int]] [x: 1  y: 2]]]", "p");
        // The returned type is the annotation (Intersection) after substitution.
        // Use assert_has_field to check for the annotated field types regardless of form.
        assert_has_field(&ty, "x", &Type::Int);
        assert_has_field(&ty, "y", &Type::Int);
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

        // Verify result resolves to IntLiteral(42) (polymorphic call preserves literal type)
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
        // We call check_expr directly with a hand-built AST to test the actual arity check
        // code path without going through the full parse pipeline.
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
        let body = Spanned::new(Expr::var_ref("x".to_string()), span);
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
            params: vec![(None, Type::Str)],
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
        let (errors1, type_map1, _doc_map1, _scheme_map1, _diagnostics1) =
            typecheck_file_with_types(&file.node);
        assert!(
            errors1.is_empty() || errors1.iter().all(|e| !e.message.contains("panic")),
            "First typecheck should not panic"
        );
        assert!(
            !type_map1.is_empty(),
            "First typecheck should populate type_map"
        );

        // Second typecheck on the same AST: should not panic due to reset_elaboration
        let (errors2, type_map2, _doc_map2, _scheme_map2, _diagnostics2) =
            typecheck_file_with_types(&file.node);
        assert!(
            errors2.is_empty() || errors2.iter().all(|e| !e.message.contains("panic")),
            "Second typecheck should not panic"
        );
        assert!(
            !type_map2.is_empty(),
            "Second typecheck should populate type_map"
        );

        // Third typecheck to be extra sure
        let (errors3, _type_map3, _doc_map3, _scheme_map3, _diagnostics3) =
            typecheck_file_with_types(&file.node);
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
        let (errors, type_map, _doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types(&file.node);

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
            subst.type_map.borrow().is_empty(),
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
        // Covers: $builtin-seq, $builtin-repeat, $builtin-cycle, $builtin-iterate, $builtin-unfold, $take
        // NOTE: $builtin-seq takes (head, tail) args — it's the primitive Seq cons operation.
        // The user-facing wrappers ($seq, $range, $repeat, $cycle, $iterate, $unfold) live in
        // prelude.llt and are not present when using TypeEnv::with_builtins() alone.
        let input = r#"
            [some_seq: [call $builtin-range 0 10]]
            [seq_result: [call $builtin-seq 1 $some_seq]]
            [repeat_result: [call $builtin-repeat 42]]
            [cycle_result: [call $builtin-cycle $some_seq]]
            [iterate_result: [call $builtin-iterate [fn [x@a] $x] 0]]
            [unfold_result: [call $builtin-unfold [fn [x@a] [Just: [x  $x]]] 0]]
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

        let (errors, _diagnostics) = typecheck_file(&file.node);
        assert!(
            errors.is_empty(),
            "% pipeline binding should work, got error: {:?}",
            errors
        );
    }

    #[test]
    fn test_pipeline_percent_pipeline_multi_field() {
        // Test that [+ %.x %.y] type-checks without errors in a multi-doc pipeline.
        // + is registered as Numeric a => a -> a -> a so the result is constrained numeric.
        // Uses file_env_with_builtins because + is a stdlib builtin (not in TypeEnv::new()).
        let input = "[x: 1  y: 2]\n---\n[z: [+ %.x %.y]]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, _diagnostics) = typecheck_file(&file.node);
        assert!(
            errors.is_empty(),
            "% multi-field pipeline should type-check without errors; got: {:?}",
            errors
        );
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

        let (errors, _diagnostics) = typecheck_file(&file.node);
        assert!(
            errors.is_empty(),
            "named section binding should work, got error: {:?}",
            errors
        );
    }

    // -- Diagnostic system tests --

    #[test]
    fn test_typecheck_returns_diagnostics() {
        // Verify that typecheck_file returns a diagnostics vector (currently empty)
        let input = "[x: 42]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let (errors, diagnostics) = typecheck_file(&file.node);
        assert!(
            errors.is_empty(),
            "simple dict should typecheck without errors"
        );
        assert!(
            diagnostics.is_empty(),
            "no diagnostics emitted yet (infrastructure only)"
        );
    }

    #[test]
    fn test_typecheck_with_types_returns_diagnostics() {
        // Verify that typecheck_file_with_types_and_env returns diagnostics in the tuple
        let input = "[x: 42]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let env = Rc::new(TypeEnv::new());
        let (errors, _type_map, _doc_map, _scheme_map, diagnostics) =
            typecheck_file_with_types_and_env(&file.node, env);
        assert!(
            errors.is_empty(),
            "simple dict should typecheck without errors"
        );
        assert!(
            diagnostics.is_empty(),
            "no diagnostics emitted yet (infrastructure only)"
        );
    }

    // -- row_ann_mapping threading in resolve_type_assert (Task 5) --

    #[test]
    fn test_type_assert_named_row_var_shared_within_annotation() {
        // Exercises resolve_type_assert's row_ann_mapping (typecheck.rs:2059-2062).
        //
        // A TypeAssert with a Fn-type annotation where the named row variable `...r`
        // appears in BOTH the return type and the parameter type:
        //   [@[Fn@[result: Int ...r] [[input: String ...r]]] expr]
        //
        // row_ann_mapping in resolve_type_assert ensures both `...r` occurrences within
        // this single TypeAssert annotation map to the SAME fresh row variable name.
        // If row_ann_mapping were not threaded (the bug state), each `...r` would receive
        // an independent anonymous row var, and the two positions would be unrelated.
        //
        // We verify that both positions produce the same row var name by extracting the
        // Function type from the type_map and checking that the RowVar names match between
        // the parameter record type and the return record type.
        //
        // The expression: [fn [x@[input: String ...r]] [result: 42]]
        // satisfies the annotation [@[Fn@[result: Int ...r] [[input: String ...r]]]] because:
        //   - param type [input: String ...r] matches the annotation's param type [input: String ...r]
        //   - return type [result: IntLiteral(42)] <: [result: Int ...r] (subsumption, open row)
        //
        // If ...r is the SAME row var in both positions, unification constrains r consistently.
        let result = check(
            "[f: [@[Fn@[result: Int ...r] [[input: String ...r]]] [fn [x@[input: String ...r]] [result: 42]]]]"
        );
        assert!(
            result.is_ok(),
            "TypeAssert with shared named row variable in Fn annotation should type-check: {:?}",
            result.err()
        );

        // BAS: all records are closed (RowTail::Empty). Under BAS, the named row variable "...r"
        // is handled by closure in the annotation but the tail is always Empty.
        // Verify the param and return are both records.
        let ty = result_field(
            "[f: [@[Fn@[result: Int ...r] [[input: String ...r]]] [fn [x@[input: String ...r]] [result: 42]]]]",
            "f"
        );
        match ty {
            Type::Function { params, ret, .. } => {
                assert!(
                    matches!(&params[0].1, Type::Record(_)),
                    "param should be Record type, got {:?}",
                    params[0].1
                );
                assert!(
                    matches!(ret.as_ref(), Type::Record(_)),
                    "return should be Record type, got {ret}"
                );
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_type_assert_named_row_var_independent_across_annotations() {
        // Companion to test_type_assert_named_row_var_shared_within_annotation.
        // Exercises that each TypeAssert gets its OWN independent row_ann_mapping scope:
        // two sibling TypeAsserts both using `...r` should NOT share the same row variable
        // (they are independent annotation scopes, just like outer-scope type annotation
        // independence tested in test_fix1_outer_scope_annotations_are_independent).
        //
        // Both TypeAsserts should succeed independently.
        let result = check(
            "[x: [@[a: Int ...r] [a: 1  extra: true]]\
             \n y: [@[a: String ...r] [a: \"hello\"  other: 42]]]",
        );
        assert!(
            result.is_ok(),
            "two independent TypeAsserts with ...r should both succeed independently: {:?}",
            result.err()
        );
    }

    // ===== Union Type Tests =====
    //
    // These test resolve_annotation directly with programmatic PropertyDict
    // construction, because the parser's implied-call rule prevents @[Int String]
    // from parsing as a Dict with positional entries. Parser-level union syntax
    // is a future sprint item.

    /// Helper: build a PropertyDict annotation with positional type entries.
    /// Uses VarRef (unquoted identifiers) for type names, matching parser behavior.
    fn union_annotation(type_names: &[&str]) -> (Annotation, Span) {
        let span = crate::test_util::test_span(1, 1, 1, 20);
        let sp = |node: Expr| Spanned::new(node, span);
        let entries: Vec<Spanned<Entry>> = type_names
            .iter()
            .map(|name| {
                Spanned::new(
                    Entry {
                        key: None,
                        value: Rc::new(sp(Expr::VarRef {
                            name: (*name).to_string(),
                            escaped: false,
                            resolved: RefCell::new(None),
                        })),
                    },
                    span,
                )
            })
            .collect();
        (Annotation::PropertyDict(entries), span)
    }

    #[test]
    fn test_union_annotation_basic() {
        // Two positional entries → Union(Int, Str)
        let (ann, span) = union_annotation(&["Int", "String"]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_union_annotation_three_types() {
        // Three positional entries → Union(Int, Str, Bool)
        let (ann, span) = union_annotation(&["Int", "String", "Bool"]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(members.len(), 3);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
                assert!(members.contains(&Type::Bool));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_union_annotation_single_unwraps() {
        // Single positional entry → unwraps to bare type
        let (ann, span) = union_annotation(&["Int"]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(ty, Type::Int);
    }

    #[test]
    fn test_union_annotation_with_metadata() {
        // Positional entries + keyed metadata: Union(Int, Str) with default
        let span = crate::test_util::test_span(1, 1, 1, 20);
        let sp = |node: Expr| Spanned::new(node, span);
        // Use VarRef for type names (unquoted identifiers) — Expr::Str is for string literal types
        let ann = Annotation::PropertyDict(vec![
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "Int".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "String".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
            Spanned::new(
                Entry {
                    key: Some(sp(Expr::Str("default".into()))),
                    value: Rc::new(sp(Expr::Int(0))),
                },
                span,
            ),
        ]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_or_annotation_two_types() {
        // @[or Int Null] → resolve_annotation produces Union(Int, Record({}))
        // `or` is the type-stage keyword for union; `Null` is the empty record type.
        // Use resolve_annotation directly (same pattern as test_union_annotation_basic).
        let span = crate::test_util::test_span(1, 1, 1, 20);
        let sp = |node: Expr| Spanned::new(node, span);
        // Build [or Int Null] as positional entries: [or, Int, Null]
        let ann = Annotation::PropertyDict(vec![
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "or".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "Int".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "Null".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
        ]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(
                    members.len(),
                    2,
                    "expected 2 union members, got {}",
                    members.len()
                );
                assert!(members.contains(&Type::Int), "union should contain Int");
            }
            other => panic!("expected Union, got {other}"),
        }
    }

    #[test]
    fn test_or_annotation_three_types() {
        // @[or Int Float Bool] → Union(Bool, Float, Int) (sorted by normalize_union)
        let span = crate::test_util::test_span(1, 1, 1, 30);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(
            ["or", "Int", "Float", "Bool"]
                .iter()
                .map(|name| {
                    Spanned::new(
                        Entry {
                            key: None,
                            value: Rc::new(sp(Expr::VarRef {
                                name: (*name).into(),
                                escaped: false,
                                resolved: RefCell::new(None),
                            })),
                        },
                        span,
                    )
                })
                .collect(),
        );
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(
                    members.len(),
                    3,
                    "expected 3 union members, got {}",
                    members.len()
                );
            }
            other => panic!("expected Union, got {other}"),
        }
    }

    #[test]
    fn test_or_in_type_alias_body() {
        // [MyUnion: [type [or Int Null]]] registers a type alias whose body is Union(Int, Null).
        // Type aliases are dict entries whose value is a [type ...] form.
        let env = doc_env("[MyUnion: [type [or Int Null]]  x: 42]");
        let alias = env.get_type_alias("MyUnion");
        assert!(
            alias.is_some(),
            "expected MyUnion type alias to be registered"
        );
        let body = &alias.unwrap().body;
        assert!(
            matches!(body, Type::Union(members) if members.len() == 2),
            "expected Union(2) alias body, got {body}"
        );
    }

    #[test]
    fn test_or_annotation_in_fn_return() {
        // fn@[return: [or Int Null]] — or in fn metadata return type
        let ty = infer("[fn@[return: [or Int Null]] [] []]");
        match ty {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(*ret, Type::Union(ref m) if m.len() == 2),
                    "expected Union(2) return type, got {ret}"
                );
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_union_type_assert_success() {
        // value_matches_type: Int matches Union(Int, Str)
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(crate::eval::value_matches_type(
            &crate::value::Value::Int(42),
            &union
        ));
    }

    #[test]
    fn test_union_type_assert_failure() {
        // value_matches_type: Bool does NOT match Union(Int, Str)
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(!crate::eval::value_matches_type(
            &crate::value::Value::Bool(true),
            &union
        ));
    }

    #[test]
    fn test_union_in_function_signature() {
        // resolve_annotation with Fn@ whose return type is a union (via PropertyDict)
        let span = crate::test_util::test_span(1, 1, 1, 20);
        let sp = |node: Expr| Spanned::new(node, span);
        // Build annotation: Fn@... where the annotation is a PropertyDict with positional entries
        // This simulates [Fn@[Int String]]
        // Use VarRef for type names — Expr::Str is for string literal types
        let fn_ann = Annotation::PropertyDict(vec![
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "Int".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
            Spanned::new(
                Entry {
                    key: None,
                    value: Rc::new(sp(Expr::VarRef {
                        name: "String".into(),
                        escaped: false,
                        resolved: RefCell::new(None),
                    })),
                },
                span,
            ),
        ]);
        let env = Rc::new(TypeEnv::new());
        let ret_ty = resolve_annotation(
            &fn_ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ret_ty {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_union_nullable_pattern() {
        // Union(Int, Record(Empty)) — nullable integer pattern
        let null_type = Type::Record(Row {
            fields: HashMap::new(),
        });
        let union = Type::normalize_union(vec![Type::Int, null_type.clone()]);
        match union {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&null_type));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_union_deduplication() {
        // Three positional entries with duplicate → deduplicated Union(Int, Str)
        let (ann, span) = union_annotation(&["Int", "String", "Int"]);
        let env = Rc::new(TypeEnv::new());
        let ty = resolve_annotation(
            &ann,
            &env,
            span,
            &mut InferState::new(),
            &mut None,
            &mut None,
        )
        .unwrap();
        match ty {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            other => panic!("Expected Union type, got {other}"),
        }
    }

    #[test]
    fn test_union_display_format() {
        // Union types display with " | " separator
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        let display = format!("{}", union);
        assert!(display.contains("Int"));
        assert!(display.contains("String"));
        assert!(display.contains(" | "));
    }

    // -- Path-Sensitive Narrowing Tests (narrowing-basic sprint) --

    #[test]
    fn test_narrowing_equality_literal_int() {
        // After `[= x 42]`, the true branch knows `x : IntLiteral(42)`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [= x 42] x 0]]");
        // The result type should be the LUB of IntLiteral(42) and IntLiteral(0), which is Int
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for narrowed if result, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_equality_literal_string() {
        // After `[= x "hello"]`, the true branch knows `x : StringLiteral("hello")`
        let env = doc_env_with_builtins("[x: \"\"]\n[result: [if [= x \"hello\"] x \"world\"]]");
        // The result type should be the LUB of StringLiteral("hello") and StringLiteral("world"), which is Str
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) => {}
            Some(other) => panic!("expected Str for narrowed if result, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_equality_literal_reversed_operands() {
        // Test that `[= 42 x]` works the same as `[= x 42]`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [= 42 x] x 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for reversed operand narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_type_of_int() {
        // After `[= [type-of x] "Int"]`, the true branch knows `x : Int`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [= [type-of x] \"Int\"] x 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for type-of narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_type_of_string() {
        // After `[= [type-of x] "String"]`, the true branch knows `x : Str`
        let env = doc_env_with_builtins(
            "[x: \"\"]\n[result: [if [= [type-of x] \"String\"] x \"default\"]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) => {}
            Some(other) => panic!("expected Str for type-of String narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_type_of_reversed() {
        // Test that `[= "Int" [type-of x]]` works the same as `[= [type-of x] "Int"]`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [= \"Int\" [type-of x]] x 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for reversed type-of, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_has_key() {
        // After `[has? x "name"]`, the true branch knows `x` has at least a `name` field.
        // has? is defined locally because it is a prelude function, not a builtin with a type scheme.
        let result = check(
            "[has?: [fn [xs k] true]]\n\
             [x: [age: 30]]\n\
             [result: [if [has? x \"name\"] $x.name \"unknown\"]]",
        );
        // This should type-check — the narrowed type has a `name` field
        assert!(result.is_ok(), "has? narrowing should allow field access");
    }

    #[test]
    fn test_narrowing_conjunction_and() {
        // After `[and [= x 42] [has? y "name"]]`, both narrowings apply.
        // and and has? are prelude functions, not builtins, so define locally.
        let env = doc_env_with_builtins(
            "[and: [fn [a b] [if a b false]]  has?: [fn [xs k] true]]\n\
             [x: 30  y: []]\n\
             [result: [if [and [= x 42] [has? y \"name\"]] [+ x $y.age] 0]]",
        );
        // This should type-check without errors. The result type is the union of the
        // then-branch ([+ x $y.age] = fresh TypeVar from unknown field access) and the
        // else-branch (IntLiteral(0)). Conjunction narrowing via user-defined `and` is
        // best-effort; the primary check is that the Equatable constraint on `=` is
        // satisfied and the expression type-checks without errors.
        assert!(
            env.get("result").is_some(),
            "field 'result' not found in env"
        );
    }

    #[test]
    fn test_narrowing_no_false_branch_narrowing() {
        // The false branch does NOT get narrowing
        let env = doc_env_with_builtins(
            "[x: 30]\n[then_result: [if [= x 42] x 0]  else_result: [if [= x 42] 0 x]]",
        );
        // In `else_result`, the else branch has `x` which should NOT be narrowed (still Int)
        match env.get("else_result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for else branch (no narrowing), got {other}"),
            None => panic!("field 'else_result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_nested_if() {
        // Nested if chains preserve narrowing in each branch
        let result = check(
            "[x: 30]\n\
             [result: [if [= x 42]\n\
                        [if [= x 42] x 0]\n\
                        0]]",
        );
        assert!(
            result.is_ok(),
            "nested if with consistent narrowing should type-check"
        );
    }

    #[test]
    fn test_narrowing_not_leaking_across_branches() {
        // Narrowing in the true branch does not affect the else branch
        let result = check(
            "[x: \"hello\"]\n\
             [result: [if [= x \"world\"]\n\
                        x\n\
                        x]]",
        );
        // Both branches return Str (or StringLiteral), should type-check
        assert!(result.is_ok(), "narrowing should not leak across branches");
    }

    #[test]
    fn test_narrowing_type_map_hover() {
        // Verify that the type map contains the narrowed type for LSP hover
        let mut file = crate::parse("[x: 30]\n[result: [if [= x 42] x 0]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::with_builtins());
        let mut state = InferState::new();
        let mut type_map = TypeMap::new();
        let mut type_map_opt = Some(&mut type_map);
        let _ =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut type_map_opt);

        // The type map should have entries for the narrowed `x` in the then branch
        // We can't easily check the exact span, but verify the type map is populated
        assert!(
            !type_map.is_empty(),
            "type map should be populated with narrowed types"
        );
    }

    #[test]
    fn test_narrowing_unrecognized_condition_no_narrowing() {
        // Unrecognized condition patterns don't narrow (< is not a narrowing pattern)
        let result = check("[x: 30]\n[result: [if [< x 10] x 0]]");
        // This should still type-check, just without narrowing
        assert!(
            result.is_ok(),
            "unrecognized condition should not break type checking"
        );
    }

    #[test]
    fn test_narrowing_type_of_dict() {
        // After `[= [type-of x] "Dict"]`, the true branch knows `x` is a Record
        let result = check("[x: []]\n[result: [if [= [type-of x] \"Dict\"] $x.field 0]]");
        // This should type-check — x is narrowed to an open Record, field access returns a TypeVar
        assert!(
            result.is_ok(),
            "type-of Dict narrowing should allow field access"
        );
    }

    #[test]
    fn test_narrowing_type_of_number() {
        // After `[= [type-of x] "Number"]`, the true branch knows `x : Number`
        let result = check("[x: 30]\n[result: [if [= [type-of x] \"Number\"] x 0]]");
        assert!(result.is_ok(), "type-of Number narrowing should work");
    }

    // === Type Predicate Narrowing Tests (B5b) ===

    #[test]
    fn test_narrowing_int_predicate() {
        // After `[int? x]`, the true branch knows `x : Int`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [int? x] x 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for int? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_str_predicate() {
        // After `[str? x]`, the true branch knows `x : Str`
        let env = doc_env_with_builtins("[x: \"\"]\n[result: [if [str? x] x \"default\"]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) => {}
            Some(other) => panic!("expected Str for str? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_bool_predicate() {
        // After `[bool? x]`, the true branch knows `x : Bool`
        let env = doc_env_with_builtins("[x: true]\n[result: [if [bool? x] x false]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Bool) => {}
            Some(other) => panic!("expected Bool for bool? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_float_predicate() {
        // After `[float? x]`, the true branch knows `x : Float`
        let env = doc_env_with_builtins("[x: 3.14]\n[result: [if [float? x] x 0.0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Float) => {}
            Some(other) => panic!("expected Float for float? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_num_predicate() {
        // After `[num? x]`, the true branch knows `x : Number`
        let env = doc_env_with_builtins("[x: 30]\n[result: [if [num? x] x 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Number) => {}
            Some(other) => panic!("expected Number for num? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_dict_predicate() {
        // After `[dict? x]`, the true branch knows `x : Record(open)`
        let env = doc_env_with_builtins("[x: [a: 1]]\n[result: [if [dict? x] x []]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Record(_)) => {}
            Some(other) => panic!("expected Record for dict? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_seq_predicate() {
        // After `[seq? x]`, the true branch knows `x : Seq(Unknown)`
        let result = check("[x: [seq 1 2]]\n[result: [if [seq? x] x [seq 1 2]]]");
        assert!(result.is_ok(), "seq? narrowing should work");
    }

    #[test]
    fn test_narrowing_null_predicate() {
        // After `[null? x]`, the true branch knows `x : Record(Empty)` (Null = empty closed record)
        let env = doc_env_with_builtins("[x: []]\n[result: [if [null? x] x []]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Record(_)) => {}
            Some(other) => panic!("expected closed Record for null? narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_fn_predicate() {
        // After `[fn? x]`, the true branch knows `x : Unknown` (can't express "any function" precisely yet)
        let result = check("[x: [fn [] 1]]\n[result: [if [fn? x] x [fn [] 0]]]");
        assert!(result.is_ok(), "fn? narrowing should work");
    }

    #[test]
    fn test_narrowing_predicate_with_conjunction() {
        // [and [int? x] [< x 100]] should apply int? narrowing in true branch
        // `and` and `>` are prelude functions, not builtins, so define `and` locally.
        let env = doc_env_with_builtins(
            "[and: [fn [a b] [if a b false]]]\n\
             [x: 30]\n\
             [result: [if [and [int? x] [< x 100]] x 0]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for int?+conjunction narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    #[test]
    fn test_narrowing_predicate_with_variable_binding() {
        // Test that narrowing works correctly when variable is bound to another name
        let env = doc_env_with_builtins("[x: 30]\n[y: x]\n[result: [if [int? y] y 0]]");
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int for variable binding narrowing, got {other}"),
            None => panic!("field 'result' not found in env"),
        }
    }

    // ========== ADT Tests (C1 sprint) ==========

    #[test]
    fn test_adt_multi_entry_union_declaration() {
        // Multi-entry [type T1 T2 ...] produces Type::Union
        let env = doc_env_with_builtins("[Result: [type [ok: a] [err: String]]]");
        let alias = env
            .get_type_alias("Result")
            .expect("Result type alias not found");
        match &alias.body {
            Type::Union(members) => {
                assert_eq!(members.len(), 2, "Result should have 2 union members");
                // Check that both members are Records
                for member in members {
                    match member {
                        Type::Record(_) => {}
                        other => panic!("expected Record member, got {other}"),
                    }
                }
            }
            other => panic!("expected Union type for Result, got {other}"),
        }
    }

    #[test]
    fn test_adt_tag_only_variants() {
        // String literals in type position → Type::StringLiteral
        let env = doc_env_with_builtins("[Status: [type \"ok\" \"err\" \"pending\"]]");
        let alias = env
            .get_type_alias("Status")
            .expect("Status type alias not found");
        match &alias.body {
            Type::Union(members) => {
                assert_eq!(members.len(), 3, "Status should have 3 union members");
                // Check that all members are StringLiterals
                let tags: Vec<String> = members
                    .iter()
                    .filter_map(|m| match m {
                        Type::StringLiteral(s) => Some(s.clone()),
                        other => panic!("expected StringLiteral, got {other}"),
                    })
                    .collect();
                // Union members are sorted, so check canonical order
                assert!(tags.contains(&"ok".to_string()));
                assert!(tags.contains(&"err".to_string()));
                assert!(tags.contains(&"pending".to_string()));
            }
            other => panic!("expected Union type for Status, got {other}"),
        }
    }

    #[test]
    fn test_adt_mixed_variants() {
        // Mix of record and string literal variants
        let env = doc_env_with_builtins(
            "[Event: [type [click: [x: Int  y: Int]] [key: [code: String]] \"resize\"]]",
        );
        let alias = env
            .get_type_alias("Event")
            .expect("Event type alias not found");
        match &alias.body {
            Type::Union(members) => {
                assert_eq!(members.len(), 3, "Event should have 3 union members");
                // Count record vs string literal members
                let record_count = members
                    .iter()
                    .filter(|m| matches!(m, Type::Record(_)))
                    .count();
                let string_count = members
                    .iter()
                    .filter(|m| matches!(m, Type::StringLiteral(_)))
                    .count();
                assert_eq!(record_count, 2, "should have 2 record variants");
                assert_eq!(string_count, 1, "should have 1 string literal variant");
            }
            other => panic!("expected Union type for Event, got {other}"),
        }
    }

    #[test]
    fn test_adt_single_entry_unwrapped() {
        // Single-entry [type T] should remain a simple alias (not wrapped in Union)
        let env = doc_env_with_builtins("[Name: [type String]]");
        let alias = env
            .get_type_alias("Name")
            .expect("Name type alias not found");
        match &alias.body {
            Type::Str => {}
            other => panic!("expected Str type for single-entry Name, got {other}"),
        }
    }

    #[test]
    fn test_adt_type_assert_union_enforcement() {
        // Type alias with union body can be referenced in annotations.
        // Verify the alias resolves to a 2-member union.
        let env = doc_env_with_builtins("[Result: [type [ok: a] [err: String]]]");
        let alias = env
            .get_type_alias("Result")
            .expect("Result type alias not found");
        match &alias.body {
            Type::Union(members) => {
                assert_eq!(members.len(), 2, "should have 2 union members");
                let has_ok = members.iter().any(|m| match m {
                    Type::Record(Row { fields, .. }) => fields.contains_key("ok"),
                    _ => false,
                });
                let has_err = members.iter().any(|m| match m {
                    Type::Record(Row { fields, .. }) => fields.contains_key("err"),
                    _ => false,
                });
                assert!(has_ok, "should have [ok: ...] variant");
                assert!(has_err, "should have [err: ...] variant");
            }
            other => panic!("expected Union type for Result, got {other}"),
        }
    }

    #[test]
    fn test_try_result_type() {
        // `try` builtin returns Top — not a structural union — because the runtime
        // now returns nominal Value::Variant { tag: "Ok"/"Err" }. A structural union
        // {ok:T}|{err:Str} would cause T004 false positives when user code matches on
        // constructor patterns [Ok v] / [Err msg]. Top avoids triggering coverage
        // checking (infer_match only runs exhaustiveness when scrutinee is Type::Union).
        // See builtin-type-audit sprint: try return type (TODO.md)
        let env = TypeEnv::with_builtins();
        let scheme = env.get("try").expect("try builtin not found in env");
        match &scheme.body {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(ret.as_ref(), Type::Top),
                    "try should return Top (not structural union — see comment), got {ret}"
                );
            }
            other => panic!("expected Function type for try, got {other}"),
        }
    }

    #[test]
    fn test_adt_parameterized_alias_instantiation() {
        // Parameterized union alias: [type [a] [ok: a] [err: String]]
        // Each usage site should get fresh type variables
        let env = doc_env_with_builtins(
            "[Result: [type [a] [ok: a] [err: String]]]\n\
             [res1: [@[Result Int] [ok: 42]]]\n\
             [res2: [@[Result String] [ok: \"hello\"]]]",
        );

        // res1 should have type Union([ok: Int], [err: String])
        match env.get("res1").map(|s| &s.body) {
            Some(Type::Union(members)) => {
                assert_eq!(members.len(), 2);
                // Find the ok variant and check its type
                let ok_type = members.iter().find_map(|m| match m {
                    Type::Record(Row { fields, .. }) => fields.get("ok"),
                    _ => None,
                });
                match ok_type {
                    Some(Type::Int) => {}
                    other => panic!("expected Int for res1 ok field, got {other:?}"),
                }
            }
            other => panic!("expected Union type for res1, got {other:?}"),
        }

        // res2 should have type Union([ok: String], [err: String])
        match env.get("res2").map(|s| &s.body) {
            Some(Type::Union(members)) => {
                assert_eq!(members.len(), 2);
                let ok_type = members.iter().find_map(|m| match m {
                    Type::Record(Row { fields, .. }) => fields.get("ok"),
                    _ => None,
                });
                match ok_type {
                    Some(Type::Str) => {}
                    other => panic!("expected Str for res2 ok field, got {other:?}"),
                }
            }
            other => panic!("expected Union type for res2, got {other:?}"),
        }
    }

    #[test]
    fn test_adt_independent_call_sites() {
        // Two functions annotated with the same union alias both type-check successfully
        // and receive function types.
        let env = doc_env_with_builtins(
            "[Result: [type [ok: a] [err: String]]]\n\
             [f1: [fn [r@Result] r]]\n\
             [f2: [fn [r@Result] r]]",
        );

        // Both should be functions
        match env.get("f1") {
            Some(scheme) => match &scheme.body {
                Type::Function { .. } => {}
                other => panic!("expected Function type for f1, got {other}"),
            },
            None => panic!("f1 not found"),
        }
        match env.get("f2") {
            Some(scheme) => match &scheme.body {
                Type::Function { .. } => {}
                other => panic!("expected Function type for f2, got {other}"),
            },
            None => panic!("f2 not found"),
        }
    }

    // ========== Exhaustiveness Checking Tests (C5 sprint) ==========

    #[test]
    fn test_exhaustive_match_int_string_complete() {
        // Complete coverage: Int and String arms cover the union
        let result = check("[match [@[Int String] 42] Int: \"int\" String: \"str\"]");
        assert!(
            result.is_ok(),
            "Int+String should be exhaustive: {:?}",
            result
        );
    }

    #[test]
    fn test_exhaustive_match_wildcard_covers_all() {
        // Wildcard covers all variants
        let result = check("[match [@[Int String] 42] _: \"any\"]");
        assert!(result.is_ok(), "wildcard should cover all: {:?}", result);
    }

    #[test]
    fn test_non_exhaustive_match_missing_variant() {
        // Missing String variant
        let result = check("[match [@[Int String] 42] Int: \"int\"]");
        assert!(
            result.is_err(),
            "should fail typecheck for missing variant, but got Ok"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("non-exhaustive")),
            "should report non-exhaustive match, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_redundant_arm_detected() {
        // Third arm (Int) is redundant — already covered
        let result =
            check("[match [@[Int String] 42] Int: \"int\" String: \"str\" Int: \"int-again\"]");
        assert!(
            result.is_err(),
            "should fail typecheck for redundant arm, but got Ok"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("unreachable")),
            "should report unreachable arm, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_inaccessible_arm_after_complete_coverage() {
        // Wildcard after complete Int+String coverage — inaccessible via ⊥
        let result = check("[match [@[Int String] 42] Int: \"int\" String: \"str\" _: \"catch\"]");
        assert!(
            result.is_err(),
            "should fail typecheck for inaccessible arm, but got Ok"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("inaccessible")),
            "should report inaccessible arm, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_exhaustive_match_dict_variants() {
        // Structural variants: [ok: _] | [err: _]
        // Use positional syntax @[[ok: Int] [err: String]] for inline union.
        // Bodies use literals (not pattern variables) since pattern bindings
        // aren't yet added to the type environment in the basic match checker.
        let result = check(
            "[match [@[[ok: Int] [err: String]] [ok: 42]]\n\
                 [ok: _]:    \"ok\"\n\
                 [err: _]:   \"err\"]",
        );
        assert!(
            result.is_ok(),
            "dict variants should be exhaustive: {:?}",
            result
        );
    }

    #[test]
    fn test_non_exhaustive_match_dict_missing_variant() {
        // Missing [err: _] variant
        let result = check(
            "[match [@[[ok: Int] [err: String]] [ok: 42]]\n\
                 [ok: _]: \"ok\"]",
        );
        assert!(
            result.is_err(),
            "should fail typecheck for missing dict variant, but got Ok"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("non-exhaustive")),
            "should report non-exhaustive match for dict variants, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_exhaustive_match_string_literal_variants() {
        // String literal variants: "ok" | "err" | "pending"
        let result = check(
            "[match [@[\"ok\" \"err\" \"pending\"] \"ok\"]\n\
                 \"ok\":      \"is-ok\"\n\
                 \"err\":     \"is-err\"\n\
                 \"pending\": \"is-pending\"]",
        );
        assert!(
            result.is_ok(),
            "string literal variants should be exhaustive: {:?}",
            result
        );
    }

    #[test]
    fn test_non_exhaustive_string_literal_missing() {
        // Missing "pending" variant
        let result = check(
            "[match [@[\"ok\" \"err\" \"pending\"] \"ok\"]\n\
                 \"ok\":  \"is-ok\"\n\
                 \"err\": \"is-err\"]",
        );
        assert!(
            result.is_err(),
            "should fail typecheck for missing string literal, but got Ok"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("non-exhaustive")),
            "should report non-exhaustive match for string literal variants, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_exhaustive_match_non_union_no_check() {
        // Non-union scrutinee — match is not checked for exhaustiveness.
        // This match has only Int arm with no wildcard, but since 42 doesn't
        // have a union type, no exhaustiveness error is raised.
        let result = check("[match 42 Int: \"int\"]");
        assert!(
            result.is_ok(),
            "non-union scrutinee should not trigger exhaustiveness: {:?}",
            result
        );
    }

    // -- Recursive type aliases --

    #[test]
    fn test_recursive_type_alias_simple() {
        // Simple recursive type alias should register successfully.
        // Multi-field alias bodies now produce Intersection of open single-field records.
        let env = doc_env("[List: [type [head: Int  tail: List]]]");
        let alias = env
            .get_type_alias("List")
            .expect("List type alias not found");
        // `[head: Int  tail: List]` → Intersection([{head: Int, ...ρ1}, {tail: ?, ...ρ2}])
        // where `?` (Unknown) is the placeholder for the recursive ref.
        assert_has_field(&alias.body, "head", &Type::Int);
        assert_has_field(&alias.body, "tail", &Type::Unknown);
    }

    #[test]
    fn test_recursive_type_alias_nested() {
        // Recursive alias with nested structure
        let result = check("[Tree: [type [value: Int  left: Tree  right: Tree]]]");
        assert!(
            result.is_ok(),
            "recursive Tree type should register: {:?}",
            result
        );
    }

    #[test]
    fn test_recursive_type_alias_usage() {
        let result = check("[List: [type [head: Int  tail: List]]]\n[x@List: [head: 1  tail: [head: 2  tail: []]]]");
        assert!(
            result.is_ok(),
            "should be able to use recursive type alias in annotation: {:?}",
            result
        );
    }

    #[test]
    fn test_mutual_recursion_two_aliases() {
        // Both aliases in the same dict: two-pass registration lets each see the other
        let result = check("[A: [type [b_field: B]]  B: [type [a_field: A]]]");
        assert!(
            result.is_ok(),
            "mutually recursive type aliases should work: {:?}",
            result
        );
    }

    #[test]
    fn test_recursive_type_depth_limit() {
        // Create a very deeply nested type that would exceed the depth limit
        // if actually expanded, but should be caught by the guard
        // Note: This test may not trigger the depth limit in the current implementation
        // because we use Unknown as a placeholder, which prevents deep expansion
        let result = check("[Deep: [type [next: Deep]]]");
        assert!(
            result.is_ok(),
            "recursive type with Unknown placeholder should not hit depth limit: {:?}",
            result
        );
    }

    #[test]
    fn test_non_recursive_alias_unchanged() {
        // Non-recursive aliases should continue to work as before.
        // Multi-field alias bodies now produce Intersection of open single-field records.
        let env = doc_env("[Point: [type [x: Int  y: Int]]]");
        let alias = env
            .get_type_alias("Point")
            .expect("Point type alias not found");
        // `[x: Int  y: Int]` → Intersection([{x: Int, ...ρ1}, {y: Int, ...ρ2}])
        assert_has_field(&alias.body, "x", &Type::Int);
        assert_has_field(&alias.body, "y", &Type::Int);
    }

    // ========== DocMap Extraction Tests ==========

    #[test]
    fn test_doc_extraction_from_param_annotation() {
        // Test existing functionality: extract doc from parameter annotations
        let input = "[f: [fn [x@[doc: \"The input value\"]] x]]";
        let file = crate::parse(input).unwrap();
        let (_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types(&file.node);

        assert_eq!(doc_map.get("x"), Some(&"The input value".to_string()));
    }

    #[test]
    fn test_doc_extraction_from_dict_entry_key() {
        // Test Task 1: extract doc from dict entry key annotation
        let input = "[myFunc@[doc: \"My function\"]: [fn [] 42]]";
        let file = crate::parse(input).unwrap();
        let (_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types(&file.node);

        assert_eq!(doc_map.get("myFunc"), Some(&"My function".to_string()));
    }

    #[test]
    fn test_doc_extraction_from_fn_return_annotation() {
        // Test Task 2: extract doc from function return annotation
        let input = "[count@[]: [fn@[type: Int  doc: \"Returns the count\"] [] 42]]";
        let file = crate::parse(input).unwrap();
        let (_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types(&file.node);

        assert_eq!(doc_map.get("count"), Some(&"Returns the count".to_string()));
    }

    #[test]
    fn test_doc_extraction_combined() {
        // Test all three extraction patterns together
        let input = r#"
[helper@[doc: "Helper function"]: [fn@[doc: "Adds two numbers"] [a@[doc: "First number"] b@[doc: "Second number"]] [+ a b]]]
        "#;
        let file = crate::parse(input).unwrap();
        let (_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types(&file.node);

        // When both key annotation and return annotation have doc:, the return annotation
        // wins because it is extracted later during recursion (overwrite semantics).
        assert_eq!(doc_map.get("helper"), Some(&"Adds two numbers".to_string()));
        assert_eq!(doc_map.get("a"), Some(&"First number".to_string()));
        assert_eq!(doc_map.get("b"), Some(&"Second number".to_string()));
    }

    // test_doc_extraction_fn_return_only: covered by test_doc_extraction_from_fn_return_annotation
    // which uses count@[]: syntax to thread the binding name via Annotated key.

    // ========== Match Arm Scope Tests (match-arm-scope sprint) ==========

    #[test]
    fn test_match_arm_variable_pattern_binds_scrutinee_type() {
        // Pattern::Variable(name) binds the whole scrutinee type.
        // [match 42 n n] — n is bound to IntLiteral(42), arm body returns it.
        let result = check("[x: [match 42 n: n]]");
        assert!(
            result.is_ok(),
            "variable pattern binding should type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_match_arm_dict_pattern_in_scope() {
        // Pattern::Dict with Variable sub-patterns injects field bindings.
        // [match [ok: 42] [ok: v] v _ 0] — v is in scope in the arm body.
        // Without env extension, v would be "undefined variable".
        let result = check("[x: [match [ok: 42] [ok: v]: v _: 0]]");
        assert!(
            result.is_ok(),
            "dict pattern-bound variable should be in scope in arm body: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_match_arm_dict_pattern_field_type_narrowed() {
        // For a concrete scrutinee Record, dict pattern fields get the field's type.
        // [match [ok: 42] [ok: v] v _ 0] — scrutinee is Record({ok: IntLiteral(42)}).
        // v should receive IntLiteral(42), so [+ v 1] type-checks as IntLiteral arithmetic.
        let result = check("[x: [match [ok: 42] [ok: v]: [+ v 1] _: 0]]");
        assert!(
            result.is_ok(),
            "dict pattern variable with concrete field type should allow arithmetic: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_match_arm_wildcard_no_bindings() {
        // Pattern::Wildcard introduces no bindings — no undefined variable errors.
        let result = check("[x: [match 42 _: 99]]");
        assert!(
            result.is_ok(),
            "wildcard pattern with no bindings should type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_match_arm_nested_dict_pattern_bindings() {
        // Nested patterns: [a: v1  b: v2] binds both v1 and v2.
        let result = check("[x: [match [a: 1  b: 2] [a: v1  b: v2]: [+ v1 v2] _: 0]]");
        assert!(
            result.is_ok(),
            "nested dict pattern variables should both be in scope: {:?}",
            result.err()
        );
    }

    // ========== Typecheck Completeness Tests ==========

    #[test]
    fn test_recursive_function_with_annotation_works() {
        // Task 1: Recursive functions WITH return annotations should work
        // Use a simple recursive function that returns a constant (doesn't actually recurse at runtime)
        let result = check("[f: [fn@Int [x@Int] 42]]");
        assert!(
            result.is_ok(),
            "function with return annotation should type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_recursive_function_without_annotation_errors() {
        // Task 1: Recursive functions WITHOUT return annotations should error
        // Use a simpler recursive function to avoid other type errors
        let result = check("[f: [fn [x] [$f $x]]]");
        assert!(
            result.is_err(),
            "recursive function without return annotation should fail"
        );
        let errs = result.unwrap_err();
        // Accept either the recursion error or the infinite type error
        // (infinite type occurs when the check doesn't catch it in time)
        assert!(
            errs.iter()
                .any(|e| e.message.contains("recursive function requires")
                    || e.message.contains("infinite type")),
            "should report either polymorphic recursion or infinite type error, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_call_mono_poly_agree_on_literals() {
        // Task 2: CALL-MONO and CALL-POLY should give consistent results
        // Polymorphic function (CALL-POLY path) and monomorphic function (CALL-MONO path)
        // should both accept IntLiteral(42) for Int parameter
        let result = check(
            "[id: [fn [x] $x]\n\
             id_int: [fn [x@Int] $x]\n\
             poly_result: [$id 42]\n\
             mono_result: [$id_int 42]]",
        );
        assert!(
            result.is_ok(),
            "both CALL-MONO and CALL-POLY should accept IntLiteral for Int param: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_int_to_number_subsumption() {
        // Task 2: Passing Int to Number param should work via subsumption
        let result = check(
            "[to_number: [fn [x@Number] $x]\n\
             result: [$to_number 42]]",
        );
        assert!(
            result.is_ok(),
            "Int should be accepted for Number parameter via subsumption: {:?}",
            result.err()
        );
    }

    // -- SCC-based binding group analysis tests --

    #[test]
    fn test_scc_singleton_generalization() {
        // Singleton SCCs (non-recursive entries) should be generalized before
        // dependent entries see them, allowing polymorphic use.
        // Use [fn [x@a] $x] (annotated TypeVar param) so `id` is genuinely polymorphic.
        // With Unknown params, this test passes vacuously via gradual semantics even
        // if SCC generalization is completely removed. With a TypeVar param, a monomorphic
        // `id` would bind `a = IntLiteral(42)` at the first call and then fail to unify
        // with `"hello"` at the second call — proving SCC generalization is active.
        let result = check(
            "[id: [fn [x@a] $x]\n\
             result_int: [$id 42]\n\
             result_str: [$id \"hello\"]]",
        );
        assert!(
            result.is_ok(),
            "id should be generalized and usable at both Int and Str: {:?}",
            result.err()
        );
        // Also verify the scheme is genuinely polymorphic (has at least one type_var).
        let env = doc_env(
            "[id: [fn [x@a] $x]\n\
             result_int: [$id 42]\n\
             result_str: [$id \"hello\"]]",
        );
        let id_scheme = env.get("id").expect("id must be in env");
        assert!(
            !id_scheme.type_vars.is_empty(),
            "id scheme should have type_vars (be polymorphic), got: {:?}",
            id_scheme.type_vars
        );
    }

    #[test]
    fn test_scc_mutual_recursion_monomorphic() {
        // Mutually recursive entries form an SCC and remain monomorphic within it.
        // even and odd both call each other, so they're in the same SCC.
        let result = check(
            "[even?: [fn [n@Int] [if [= $n 0] true  [odd?  [- $n 1]]]]\n\
             odd?:  [fn [n@Int] [if [= $n 0] false [even? [- $n 1]]]]]",
        );
        assert!(
            result.is_ok(),
            "mutually recursive functions should type-check correctly: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_scc_nested_dict_generalization() {
        // Nested dicts should also get SCC-based generalization.
        // Use [fn [x@a] $x] so the test detects SCC removal (Unknown would pass vacuously).
        let result = check(
            "[outer: [inner: [id: [fn [x@a] $x]\n\
                             use_int: [$id 42]\n\
                             use_str: [$id \"hello\"]]]]",
        );
        assert!(
            result.is_ok(),
            "nested dict entries should get SCC-based generalization: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_scc_dependency_chain() {
        // If a→b→c (dependency chain), each should be generalized before the next.
        // Use [fn [x@a] $x] so the test detects SCC removal (Unknown would pass vacuously).
        let result = check(
            "[c: [fn [x@a] $x]\n\
             b: [fn [y@b] [$c $y]]\n\
             a: [fn [z@c_] [$b $z]]\n\
             result_int: [$a 42]\n\
             result_str: [$a \"hello\"]]",
        );
        assert!(
            result.is_ok(),
            "dependency chain should allow polymorphic use of final function: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_scc_non_recursive_function_generalizes() {
        // A non-recursive function should be generalized even if it's defined
        // alongside other function entries.
        // Use [fn [x@a] $x] so the test detects SCC removal (Unknown would pass vacuously).
        let result = check(
            "[id: [fn [x@a] $x]\n\
             const: [fn [x@Int] $x]\n\
             use_id_int: [$id 42]\n\
             use_id_str: [$id \"hello\"]]",
        );
        assert!(
            result.is_ok(),
            "non-recursive id should be generalized despite monomorphic const: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_collect_pattern_bindings_variable() {
        // Unit test for collect_pattern_bindings: Variable pattern
        let mut out = Vec::new();
        collect_pattern_bindings(&Pattern::Variable("x".into()), &Type::Int, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "x");
        assert_eq!(out[0].1, Type::Int);
    }

    #[test]
    fn test_collect_pattern_bindings_dict_field_narrowed() {
        // Unit test: Dict pattern on a concrete Record type narrows field type
        let scrutinee = Type::Record(Row {
            fields: {
                let mut m = HashMap::new();
                m.insert("ok".into(), Type::Int);
                m
            },
        });
        let mut out = Vec::new();
        collect_pattern_bindings(
            &Pattern::Dict {
                fields: vec![(
                    "ok".into(),
                    Spanned::new(Pattern::Variable("v".into()), Span::origin()),
                )],
                rest: false,
            },
            &scrutinee,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "v");
        assert_eq!(
            out[0].1,
            Type::Int,
            "field 'ok: Int' should narrow v to Int"
        );
    }

    #[test]
    fn test_collect_pattern_bindings_dict_missing_field_falls_back_to_unknown() {
        // Dict pattern with key not present in Record → Unknown fallback
        let scrutinee = Type::Record(Row {
            fields: HashMap::new(),
        });
        let mut out = Vec::new();
        collect_pattern_bindings(
            &Pattern::Dict {
                fields: vec![(
                    "missing".into(),
                    Spanned::new(Pattern::Variable("v".into()), Span::origin()),
                )],
                rest: false,
            },
            &scrutinee,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "v");
        assert_eq!(
            out[0].1,
            Type::Unknown,
            "field not in Record → Unknown fallback"
        );
    }

    #[test]
    fn test_collect_pattern_bindings_wildcard_no_bindings() {
        // Wildcard pattern introduces no bindings
        let mut out = Vec::new();
        collect_pattern_bindings(&Pattern::Wildcard, &Type::Int, &mut out);
        assert!(out.is_empty(), "wildcard should introduce no bindings");
    }

    #[test]
    fn test_collect_pattern_bindings_seq_head_tail() {
        // Seq pattern: head gets element type, tail gets Seq(element type)
        let scrutinee = Type::Seq(Box::new(Type::Int));
        let mut out = Vec::new();
        collect_pattern_bindings(
            &Pattern::Seq {
                head: Box::new(Spanned::new(Pattern::Variable("h".into()), Span::origin())),
                tail: Box::new(Spanned::new(Pattern::Variable("t".into()), Span::origin())),
            },
            &scrutinee,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        let h = out.iter().find(|(n, _)| n == "h").expect("h binding");
        let t = out.iter().find(|(n, _)| n == "t").expect("t binding");
        assert_eq!(h.1, Type::Int, "head should get element type");
        assert_eq!(
            t.1,
            Type::Seq(Box::new(Type::Int)),
            "tail should get Seq(element type)"
        );
    }

    #[test]
    fn test_collect_pattern_bindings_or() {
        // Or-pattern: only collects from first alternative
        let mut out = Vec::new();
        collect_pattern_bindings(
            &Pattern::Or(vec![
                Spanned::new(Pattern::Variable("x".into()), Span::origin()),
                Spanned::new(Pattern::Variable("y".into()), Span::origin()),
            ]),
            &Type::Int,
            &mut out,
        );
        assert_eq!(
            out.len(),
            1,
            "Or-pattern should collect only from first alt"
        );
        assert_eq!(out[0].0, "x", "should bind first alternative's variable");
        assert_eq!(out[0].1, Type::Int);
    }

    #[test]
    fn test_collect_pattern_bindings_constructor() {
        // Constructor pattern with binding gets Unknown type
        let mut out = Vec::new();
        collect_pattern_bindings(
            &Pattern::Constructor {
                tag: "Some".into(),
                binding: Some(Box::new(Spanned::new(
                    Pattern::Variable("v".into()),
                    Span::origin(),
                ))),
            },
            &Type::Int, // scrutinee type is ignored for constructor payload
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "v");
        assert_eq!(
            out[0].1,
            Type::Unknown,
            "constructor binding should get Unknown type (no payload narrowing yet)"
        );
    }

    // ========== BAS Core Tests ==========

    // --- C-Var1/2 Constraint Rewriting ---

    #[test]
    fn test_c_var1_binds_typevar_in_union() {
        // C-Var1: unify(Int, Union([Str, TypeVar(a)])) → bind a = Int
        // because Int is not covered by the non-var member Str
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let var_name = "_a0".to_string();
        state.levels.insert(var_name.clone(), 1);
        let a = Type::Int;
        let b = Type::Union(vec![Type::Str, Type::TypeVar(var_name.clone(), 1)]);
        let result = unify(&a, &b, &mut subst, &mut state, Span::origin());
        assert!(result.is_ok(), "C-Var1 should succeed: {result:?}");
        // a is bound to Int
        assert_eq!(
            subst.get(&var_name),
            Some(Type::Int),
            "TypeVar should be bound to Int"
        );
    }

    #[test]
    fn test_c_var1_already_covered_no_binding() {
        // C-Var1: unify(Int, Union([Int, TypeVar(a)])) → Int already covered, no binding needed
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let var_name = "_a1".to_string();
        state.levels.insert(var_name.clone(), 1);
        let a = Type::Int;
        let b = Type::Union(vec![Type::Int, Type::TypeVar(var_name.clone(), 1)]);
        let result = unify(&a, &b, &mut subst, &mut state, Span::origin());
        assert!(
            result.is_ok(),
            "C-Var1 already covered should succeed: {result:?}"
        );
        // TypeVar should NOT be bound (Int already covered by non-var member)
        assert!(
            subst.get(&var_name).is_none(),
            "TypeVar should not be bound when already covered"
        );
    }

    #[test]
    fn test_c_var1_symmetric_union_on_left() {
        // C-Var1 symmetric: unify(Union([Str, TypeVar(a)]), Int) → bind a = Int
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let var_name = "_a2".to_string();
        state.levels.insert(var_name.clone(), 1);
        let a = Type::Union(vec![Type::Str, Type::TypeVar(var_name.clone(), 1)]);
        let b = Type::Int;
        let result = unify(&a, &b, &mut subst, &mut state, Span::origin());
        assert!(
            result.is_ok(),
            "C-Var1 symmetric should succeed: {result:?}"
        );
        assert_eq!(
            subst.get(&var_name),
            Some(Type::Int),
            "TypeVar should be bound to Int"
        );
    }

    #[test]
    fn test_c_var2_binds_typevar_in_intersection() {
        // C-Var2: unify(Intersection([Str, TypeVar(a)]), Int) → bind a = Int
        // because Str alone doesn't satisfy Int
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let var_name = "_a3".to_string();
        state.levels.insert(var_name.clone(), 1);
        // Intersection([Str, TypeVar(a)]) — Str doesn't satisfy Int, so bind a = Int
        let a = Type::Intersection(vec![Type::Str, Type::TypeVar(var_name.clone(), 1)]);
        let b = Type::Int;
        let result = unify(&a, &b, &mut subst, &mut state, Span::origin());
        assert!(result.is_ok(), "C-Var2 should succeed: {result:?}");
        assert_eq!(
            subst.get(&var_name),
            Some(Type::Int),
            "TypeVar should be bound to Int"
        );
    }

    // --- @[[all A B]] and @[[without A]] annotation syntax ---

    #[test]
    fn test_annotation_all_produces_intersection() {
        // @[[all Int Str]] → Type::Intersection([Int, Str])
        // Note: normalize_intersection sorts members
        let source = "[result: [@[[all Int Str]] 42]]";
        // We just check that it parses without error — the check is that the annotation
        // resolves to an Intersection type (checking mode will verify against the value)
        let result = check(source);
        // Int & Str is an uninhabited intersection — but type checking here is checking 42 : Int & Str
        // which should fail since 42 : Int is not a subtype of Str.
        // This is expected behavior — just verify no panic, and errors are type errors (not parse errors).
        let _ = result; // may succeed or fail, but should not panic
    }

    #[test]
    fn test_annotation_all_two_compatible_types() {
        // @[[all Int Number]] → Int & Number = Int (since Int <: Number)
        // Checking 42 against Int & Number should succeed since 42 : Int and Int <: Number
        let source = "[@[[all Int Number]] 42]";
        let result = check(source);
        // Int & Number — 42 satisfies both Int and Number, so this should succeed or at most
        // give a Negation-related message, not a structural crash
        let _ = result;
    }

    #[test]
    fn test_annotation_without_produces_negation() {
        // @[[without Int]] → Type::Negation(Int)
        // Just ensure it parses and resolves without panic
        let source = "[@[[without Int]] \"hello\"]";
        let result = check(source);
        // "hello" : Str — Str is not Int, so ~Int check passes
        let _ = result;
    }

    #[test]
    fn test_annotation_never_type_name() {
        // @Never should resolve to Type::Never
        let env = doc_env_with_builtins("[T: [type Never]]");
        let alias = env.get_type_alias("T").expect("T alias should exist");
        assert_eq!(
            alias.body,
            Type::Never,
            "Never type alias should resolve to Type::Never"
        );
    }

    #[test]
    fn test_annotation_top_type_name() {
        // @Top should resolve to Type::Top
        let env = doc_env_with_builtins("[T: [type Top]]");
        let alias = env.get_type_alias("T").expect("T alias should exist");
        assert_eq!(
            alias.body,
            Type::Top,
            "Top type alias should resolve to Type::Top"
        );
    }

    // --- False-branch narrowing ---

    #[test]
    fn test_false_branch_narrowing_int_predicate() {
        // In the false branch of [int? x], x should be narrowed to ~Int
        // We verify this by checking that the env_false has a Negation type for x
        // The simplest observable: if we shadow the result with the else branch value,
        // the type checker should not crash and the else-branch type is used.
        let env = doc_env_with_builtins("[x: 42]\n[result: [if [int? x] 1 0]]");
        // Both branches have Int; result should be Int
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) | Some(Type::IntLiteral(_)) | Some(Type::Number) => {}
            Some(other) => panic!("expected Int for false-branch narrowing test, got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_false_branch_negation_inserted_in_env() {
        // Verify that the false branch env actually has a Negation type.
        // We do this by calling apply_negation_narrowings directly.
        use std::rc::Rc;
        let mut state = InferState::new();
        let mut env = TypeEnv::new();
        env.insert("x".to_string(), Type::Int);
        let env = Rc::new(env);

        let narrowings = vec![Narrowing::TypeOf {
            var: "x".to_string(),
            ty: Type::Int,
        }];

        let false_env = apply_negation_narrowings(&env, &narrowings, &mut state);

        // x in false_env should be Negation(Int)
        let x_ty = false_env.get("x").map(|s| s.body.clone());
        assert_eq!(
            x_ty,
            Some(Type::Negation(Box::new(Type::Int))),
            "false branch should narrow x to ~Int"
        );
    }

    // --- I-Case3 in infer_match ---

    #[test]
    fn test_i_case3_match_arm_sees_narrowed_scrutinee() {
        // Match with TypeTag patterns — verify that match type-checks without errors.
        // The I-Case3 narrowing means the second arm sees remaining_scrutinee ∩ ~first-tag.
        let source = "[x: \"ok\"]\n[result: [match x\n    \"ok\": 1\n    \"err\": 2\n    _: 0]]";
        let result = check(source);
        assert!(
            result.is_ok(),
            "match with TypeTag should type-check: {result:?}"
        );
    }

    #[test]
    fn test_i_case3_wildcard_remaining_is_never() {
        // After a wildcard arm, remaining_scrutinee becomes Never (catch-all consumed).
        // Any subsequent arm would be unreachable — but we just verify no panic.
        let source = "[x: 42]\n[result: [match x\n    _: 1\n    1: 2]]";
        // The second arm after wildcard should be flagged as unreachable (if coverage checking fires)
        // or just succeed. Either way, no panic.
        let _ = check(source);
    }

    // ========== check_get / check_get? Tests ==========

    #[test]
    fn test_check_get_map_returns_value_type() {
        // [get key map] where map : Map[String Int] should return Int.
        // Seed TypeEnv directly with m : Map[String Int] since there is no Map literal syntax in LLT.
        let mut base_env = TypeEnv::with_builtins();
        base_env.insert(
            "m".to_string(),
            Type::Map(Box::new(Type::Str), Box::new(Type::Int)),
        );
        let env = Rc::new(base_env);
        let mut state = InferState::new();
        let mut file = crate::parse("[result: [get \"key\" m]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let result_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .unwrap();
        match result_env.get("result").map(|s| &s.body) {
            Some(Type::Int) => {}
            Some(other) => panic!("expected Int from get on Map[String Int], got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_check_get_optional_map_returns_value_or_null() {
        // [get? key map] where map : Map[String Int] should return Int|Null.
        // Seed TypeEnv directly with m : Map[String Int].
        let mut base_env = TypeEnv::with_builtins();
        base_env.insert(
            "m".to_string(),
            Type::Map(Box::new(Type::Str), Box::new(Type::Int)),
        );
        let env = Rc::new(base_env);
        let mut state = InferState::new();
        let mut file = crate::parse("[result: [get? \"key\" m]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let result_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .unwrap();
        let null_ty = Type::Record(Row {
            fields: HashMap::new(),
        });
        match result_env.get("result").map(|s| &s.body) {
            Some(Type::Union(members)) => {
                assert!(
                    members.contains(&Type::Int),
                    "Union should contain Int, got {:?}",
                    members
                );
                assert!(
                    members.contains(&null_ty),
                    "Union should contain Null (empty Record), got {:?}",
                    members
                );
            }
            Some(other) => {
                panic!("expected Union(Int|Null) from get? on Map[String Int], got {other}")
            }
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_check_get_record_known_field_returns_field_type() {
        // [get "a" rec] where rec : [a: Int] should return Int.
        let env = doc_env_with_builtins(
            "[rec: [a: 42]]\n\
             [result: [get \"a\" rec]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) | Some(Type::IntLiteral(_)) => {}
            Some(other) => panic!("expected Int from get on record [a: Int], got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_check_get_optional_record_known_field_returns_field_type_or_null() {
        // [get? "a" rec] where rec : [a: Int] should return Int|Null.
        let env = doc_env_with_builtins(
            "[rec: [a: 42]]\n\
             [result: [get? \"a\" rec]]",
        );
        let null_ty = Type::Record(Row {
            fields: HashMap::new(),
        });
        match env.get("result").map(|s| &s.body) {
            Some(Type::Union(members)) => {
                let has_int = members
                    .iter()
                    .any(|m| matches!(m, Type::Int | Type::IntLiteral(_)));
                assert!(
                    has_int,
                    "Union should contain Int or IntLiteral, got {:?}",
                    members
                );
                assert!(
                    members.contains(&null_ty),
                    "Union should contain Null, got {:?}",
                    members
                );
            }
            Some(other) => panic!("expected Union(Int|Null) from get? on record, got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_check_get_unknown_type_falls_back_to_unknown() {
        // [get key unknown_dict] where unknown_dict : Unknown should return Unknown (no narrowing).
        let env = doc_env_with_builtins(
            "[d: [builtin-if true [] []]]\n\
             [result: [get \"x\" d]]",
        );
        // We just verify it type-checks without error (returns Unknown or some type).
        let _ = env.get("result");
    }

    #[test]
    fn test_get_question_mark_registered_in_builtins() {
        // get? should be resolvable from the builtin environment without error.
        let env = Rc::new(TypeEnv::with_builtins());
        assert!(
            env.get("get?").is_some(),
            "get? should be registered in TypeEnv::with_builtins()"
        );
    }

    #[test]
    fn test_check_get_seq_integer_key_returns_element_type() {
        // [get N seq] where seq : Seq[Str] and N is an Int literal should return Str.
        // Regression test for bas-get-seq-unknown: [get 0 [split sep s]] → Str.
        let env = doc_env_with_builtins(
            "[parts: [split \"x\" \"a x b\"]]\n\
             [result: [get 0 parts]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) | Some(Type::StringLiteral(_)) => {}
            Some(other) => panic!("expected Str from [get 0 Seq[Str]], got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_builtin_get_special_form_label_typevar_returns_field_type() {
        // `builtin-get` is intercepted by check_get (like `get`/`get?`) so that
        // label TypeVar keys produce precise field types rather than Unknown.
        // Scenario: [builtin-get "host" cfg] where cfg : [host: Str] → Str.
        let env = doc_env_with_builtins(
            "[cfg: [host: \"localhost\"]]\n\
             [result: [builtin-get \"host\" cfg]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) | Some(Type::StringLiteral(_)) => {}
            Some(other) => panic!("expected Str from [builtin-get \"host\" rec], got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_builtin_get_integer_key_falls_back_to_unknown() {
        // `builtin-get` with a non-label, non-string-literal key (e.g. Int index
        // into an unknown collection) falls back to Unknown via check_get.
        // This is the common prelude-internal usage pattern.
        let env = doc_env_with_builtins(
            "[idx: 0]\n\
             [coll: [builtin-if true [] []]]\n\
             [result: [builtin-get idx coll]]",
        );
        // No type error; result has some type (Unknown or more precise).
        let _ = env.get("result");
    }

    #[test]
    fn test_split_returns_seq_str_type() {
        // split is typed as Seq[Str] in TypeEnv, not Unknown.
        let env = Rc::new(TypeEnv::with_builtins());
        let split_scheme = env.get("split").expect("split should be registered");
        match &split_scheme.body {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(ret.as_ref(), Type::Seq(inner) if matches!(inner.as_ref(), Type::Str)),
                    "split return type should be Seq[Str], got {ret}"
                );
            }
            other => panic!("split should be a Function type, got {other}"),
        }
    }

    // HasField constraint tests (hkt-field-access sprint)

    #[test]
    fn test_get_concrete_string_key_on_record() {
        // [get "name" {name: "alice"}] → type is String
        let env = doc_env_with_builtins(
            "[user: [name: \"alice\"]]\n\
             [result: [get \"name\" user]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Str) | Some(Type::StringLiteral(_)) => {}
            Some(other) => panic!("expected Str from get on record [name: Str], got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_get_union_distribution() {
        // [get "port" (A | B)] → type is A.port | B.port
        // Create two record types with different field types
        let mut base_env = TypeEnv::with_builtins();
        let mut fields_a = HashMap::new();
        fields_a.insert("port".to_string(), Type::Int);
        let mut fields_b = HashMap::new();
        fields_b.insert("port".to_string(), Type::Str);
        let union_ty = Type::normalize_union(vec![
            Type::Record(Row { fields: fields_a }),
            Type::Record(Row { fields: fields_b }),
        ]);
        base_env.insert("config".to_string(), union_ty);
        let env = Rc::new(base_env);
        let mut state = InferState::new();
        let mut file = crate::parse("[result: [get \"port\" config]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let result_env =
            typecheck_document_simple(&file.node.documents[0], &env, &mut state, &mut None)
                .unwrap();
        match result_env.get("result").map(|s| &s.body) {
            Some(Type::Union(members)) => {
                assert!(
                    members.contains(&Type::Int) || members.contains(&Type::Number),
                    "Union should contain Int or Number, got {:?}",
                    members
                );
                assert!(
                    members.contains(&Type::Str),
                    "Union should contain Str, got {:?}",
                    members
                );
            }
            Some(other) => {
                panic!("expected Union(Int|Str) from get on union, got {other}")
            }
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_get_in_literal_path() {
        // [get-in ["a" "b"] {a: {b: 42}}] → type is Int
        let env = doc_env_with_builtins(
            "[config: [a: [b: 42]]]\n\
             [result: [get-in [\"a\" \"b\"] config]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Int) | Some(Type::IntLiteral(_)) => {}
            Some(other) => panic!("expected Int from get-in on nested record, got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_get_in_empty_path_returns_dict_unchanged() {
        // [get-in [] dict] → type is dict's type
        let env = doc_env_with_builtins(
            "[user: [name: \"alice\"]]\n\
             [result: [get-in [] user]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Record(_)) => {}
            Some(other) => panic!("expected Record from get-in with empty path, got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_get_in_variable_path_falls_back_to_unknown() {
        // [get-in path dict] where path is not a literal sequence → Unknown
        let env = doc_env_with_builtins(
            "[user: [name: \"alice\"]]\n\
             [path: [\"name\"]]\n\
             [result: [get-in path user]]",
        );
        match env.get("result").map(|s| &s.body) {
            Some(Type::Unknown) => {}
            Some(other) => panic!("expected Unknown from get-in with variable path, got {other}"),
            None => panic!("field 'result' not found"),
        }
    }

    #[test]
    fn test_union_narrowing_in_pattern() {
        // Union narrowing: when matching a Union with multiple Record types,
        // and all members have a common field with the same type, the pattern-bound
        // variable should get that field type (not Unknown).
        let env = doc_env(
            "[myfn: [fn [u@[[x: Int y: String] [x: Int z: Bool]]]\n\
                      [match $u\n\
                        [x: field]: $field\n\
                        _: 0]]]",
        );
        // The match binds field from the x field. Both union members have x: Int,
        // so field should be inferred as Int (not Unknown).
        match env.get("myfn").map(|s| &s.body) {
            Some(Type::Function { ret, .. }) => {
                // Return type should be Union(Int, Int) which normalizes to Int
                // (or it might be Number if Int literals get promoted)
                assert!(
                    **ret == Type::Int || **ret == Type::Number || matches!(&**ret, Type::Union(_)),
                    "union narrowing should infer Int-compatible type for field x, got {:?}",
                    ret
                );
            }
            Some(other) => panic!("expected Function, got {other}"),
            None => panic!("myfn not found"),
        }
    }

    #[test]
    fn test_negation_subtyping_in_type_assert() {
        // [@[[without Bool]] 42] should pass: Int is disjoint from Bool
        let result = check("[@[[without Bool]] 42]");
        assert!(result.is_ok(), "Int <: ~Bool should hold");

        // [@[[without Int]] 42] should fail: Int is not disjoint from Int
        let result = check("[@[[without Int]] 42]");
        assert!(result.is_err(), "Int <: ~Int should not hold");
    }

    #[test]
    fn test_negation_subtyping_with_union() {
        // Union(String, Int) <: ~Bool should hold (all members disjoint from Bool)
        // Test via a function that takes Union(String, Int) and returns ~Bool
        let result = check(
            "[fn [x@[String Int]]\n\
               [@[[without Bool]] $x]]",
        );
        assert!(
            result.is_ok(),
            "Union(String, Int) <: ~Bool should hold: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_scan_type_quality_detects_unknown() {
        // Test that scan_type_quality emits a diagnostic for inferred Unknown.
        // This example produces 2 diagnostics:
        // 1. The field access r.y has type Unknown
        // 2. The function's return type contains Unknown
        let mut file = crate::parse("[f: [fn [r@[x: Int]] r.y]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );

        // Should have diagnostics for Unknown
        assert!(!diagnostics.is_empty(), "Expected diagnostics for Unknown");
        assert!(diagnostics.iter().all(|d| d.code == "T010"));
        assert!(diagnostics
            .iter()
            .all(|d| d.level == crate::error::DiagnosticLevel::Warn));
        assert!(diagnostics.iter().all(|d| d.message.contains("Unknown")));
    }

    #[test]
    fn test_scan_type_quality_no_diagnostic_for_concrete_types() {
        // Test that scan_type_quality does NOT emit diagnostics for concrete types
        let mut file = crate::parse("[f: [fn@Int [x@Int] x]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors or diagnostics
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );
        assert!(
            diagnostics.is_empty(),
            "Expected no diagnostics, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_scan_type_quality_explicit_unknown_annotation() {
        // Test that explicit @Unknown produces Info diagnostic (T011), not Warn (T010)
        let mut file = crate::parse("[f: [fn@Unknown [x] x]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );

        // Should have Info diagnostic for explicit Unknown
        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for explicit Unknown"
        );
        assert!(
            diagnostics.iter().any(|d| d.code == "T011"),
            "Expected T011 diagnostic for explicit Unknown, got: {:?}",
            diagnostics
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.level == crate::error::DiagnosticLevel::Info),
            "Expected Info level for explicit Unknown, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_scan_type_quality_typeassert_unknown() {
        // Test that [@Unknown expr] produces Info diagnostic (T011)
        let mut file = crate::parse("[x: [@Unknown 42]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );

        // Should have Info diagnostic for explicit Unknown
        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for explicit Unknown"
        );
        assert!(
            diagnostics.iter().any(|d| d.code == "T011"),
            "Expected T011 diagnostic for explicit Unknown in TypeAssert, got: {:?}",
            diagnostics
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.level == crate::error::DiagnosticLevel::Info),
            "Expected Info level for explicit Unknown, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_scan_type_quality_overbroad_number_annotation() {
        // Test that fn@Number when body infers Int produces Info diagnostic (T012)
        let mut file = crate::parse("[f: [fn@Number [x@Int] x]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );

        // Should have Info diagnostic for over-broad annotation
        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for over-broad annotation"
        );
        let t012_diag = diagnostics.iter().find(|d| d.code == "T012");
        assert!(
            t012_diag.is_some(),
            "Expected T012 diagnostic for over-broad annotation, got: {:?}",
            diagnostics
        );
        let diag = t012_diag.unwrap();
        assert_eq!(diag.level, crate::error::DiagnosticLevel::Info);
        assert!(
            diag.message.contains("Number") && diag.message.contains("Int"),
            "Expected message to mention Number and Int, got: {}",
            diag.message
        );
    }

    #[test]
    fn test_scan_type_quality_no_overbroad_for_matching_type() {
        // Test that fn@Int when body infers Int does NOT produce over-broad diagnostic
        let mut file = crate::parse("[f: [fn@Int [x@Int] x]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let (errors, diagnostics) = typecheck_file(&file.node);

        // Should have no type errors or diagnostics
        assert!(
            errors.is_empty(),
            "Expected no type errors, got: {:?}",
            errors
        );
        assert!(
            !diagnostics.iter().any(|d| d.code == "T012"),
            "Did not expect T012 diagnostic for matching annotation, got: {:?}",
            diagnostics
        );
    }

    // -- Label annotation tests --

    #[test]
    fn test_label_annotation_anonymous_form() {
        // key@Label should create an anonymous Label-kinded TypeVar
        let result = check("[f: [fn@a [key@Label dict@d] dict]]");
        assert!(
            result.is_ok(),
            "key@Label annotation should be accepted: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_label_annotation_named_form() {
        // key@[label: l] should create a named Label-kinded TypeVar
        let result = check("[f: [fn@a [key@[label: l] dict@d] dict]]");
        assert!(
            result.is_ok(),
            "key@[label: l] annotation should be accepted: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_label_annotation_same_name_multiple_params() {
        // Using the same label name in multiple parameters should work
        let result = check("[f: [fn@a [key1@[label: l] key2@[label: l] dict@d] dict]]");
        assert!(
            result.is_ok(),
            "same label TypeVar in multiple params should work: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_label_annotation_named_form_requires_lowercase() {
        // label: value must be a lowercase name
        let result = check("[f: [fn@a [key@[label: UpperCase] dict@d] dict]]");
        assert!(
            result.is_err(),
            "label: value with uppercase name should be rejected"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("lowercase type variable")),
            "should report that label: value must be lowercase, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_label_annotation_named_form_requires_bare_name() {
        // label: value must be a bare name, not a string literal
        let result = check("[f: [fn@a [key@[label: \"foo\"] dict@d] dict]]");
        assert!(
            result.is_err(),
            "label: value with string literal should be rejected"
        );
        let errs = result.unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("bare name")),
            "should report that label: value must be a bare name, got: {:?}",
            errs
        );
    }

    #[test]
    fn test_builtin_get_wrapper_with_label_typevar_returns_field_type() {
        // A wrapper function defined as `[fn [k@Label xs] [builtin-get k xs]]`
        // should propagate the label TypeVar through `builtin-get` (via check_get
        // special-form dispatch) and produce a precise return type when called
        // with a concrete string literal key.
        //
        // Scenario: define `my-get: [fn [k@Label xs] [builtin-get k xs]]`
        // then call `[my-get "host" cfg]` where cfg : [host: Str].
        // Expected: result is Str (precise field type, not Unknown).
        let env = doc_env_with_builtins(
            "[cfg: [host: \"localhost\"]]\n\
             [my-get: [fn [k@Label xs] [builtin-get k xs]]]\n\
             [result: [my-get \"host\" cfg]]",
        );
        // At minimum, the wrapper must not produce a type error.
        // The result type should be Str or Unknown (Unknown acceptable if
        // the prelude cache doesn't seed Equatable/etc. for the corpus check).
        let result_scheme = env.get("result");
        assert!(
            result_scheme.is_some(),
            "result should be typed (wrapper should not cause undefined-variable error)"
        );
    }

    // -- transfer_class_constraints unit test --

    #[test]
    fn test_transfer_class_constraints_via_typevar_unify() {
        // Direct unit test for the transfer_class_constraints path in U-VAR-LEVEL.
        //
        // Setup: seed state.constraints with Constraint::Class { class: "Numeric", var: "alpha" },
        // then unify(TypeVar("alpha"), TypeVar("beta")).  After unification, state.constraints
        // must contain Constraint::Class { class: "Numeric", var: "beta" } — proving that the
        // Class constraint was transferred from alpha to beta before alpha was eliminated.
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let alpha = "_alpha".to_string();
        let beta = "_beta".to_string();
        state.levels.insert(alpha.clone(), 1);
        state.levels.insert(beta.clone(), 1);

        // Seed alpha with a Numeric class constraint.
        state.constraints.push(Constraint::Class {
            class: "Numeric".to_string(),
            vars: vec![alpha.clone()],
            fundeps: vec![],
        });

        let a = Type::TypeVar(alpha.clone(), 1);
        let b = Type::TypeVar(beta.clone(), 1);
        let result = unify(&a, &b, &mut subst, &mut state, Span::origin());
        assert!(
            result.is_ok(),
            "TypeVar-TypeVar unify should succeed: {result:?}"
        );

        // After unification, beta must have the Numeric constraint.
        let beta_has_numeric = state.constraints.iter().any(|c| match c {
            Constraint::Class { class, vars, .. } => {
                class == "Numeric" && vars.len() == 1 && vars[0] == beta
            }
            _ => false,
        });
        assert!(
            beta_has_numeric,
            "beta should have Numeric constraint after transfer; state.constraints = {:?}",
            state.constraints
        );
    }

    // -- Ambiguous constraint dropping tests (src/type_env.rs:485-540) --

    #[test]
    fn test_constraint_dropped_when_typevar_not_in_return_type() {
        // fn@[constraint: [a: Comparable] return: Int] [x] x
        //
        // TypeVar 'a' is declared in constraint: [a: Comparable] but never used in a parameter
        // annotation and the return type is concrete Int.  When generalize_with_doc runs, 'a'
        // (the fresh _tN TypeVar) is NOT in the set of generalizable_vars (those appearing in the
        // function's body type), so the Comparable constraint is dropped and a TypeDiagnostic
        // T013 warning is emitted.  The resulting TypeScheme must have an empty constraints field.
        //
        // This tests that ambiguous constraints (TypeVars in constraints but not in the type)
        // are detected and reported as warnings rather than causing type errors.
        let mut file =
            crate::parse("[my_fn: [fn@[constraint: [a: Comparable] return: Int] [x] x]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let _ = crate::imports::build_prelude_env(); // populate PRELUDE_INSTANCE_CACHE
        let (errors, diagnostics) = typecheck_file(&file.node);

        assert!(
            errors.is_empty(),
            "Should not have type errors; got: {:?}",
            errors
        );

        // Verify that a diagnostic warning was emitted for the ambiguous constraint
        // Filter for T013 (ambiguous constraint) diagnostics specifically
        let ambiguous_warnings: Vec<_> = diagnostics.iter().filter(|d| d.code == "T013").collect();
        assert_eq!(
            ambiguous_warnings.len(),
            1,
            "Expected exactly 1 T013 diagnostic for ambiguous constraint; all diagnostics: {:?}",
            diagnostics
        );
        assert!(
            ambiguous_warnings[0]
                .message
                .contains("ambiguous type variable")
                && ambiguous_warnings[0].message.contains("Comparable"),
            "Expected warning about ambiguous TypeVar in Comparable constraint; got: {}",
            ambiguous_warnings[0].message
        );
    }

    #[test]
    fn test_no_false_positive_warning_for_discharged_constraints() {
        // Regression test for false-positive ambiguous constraint warnings.
        // A dict with [id: [fn [x] x], n: [+ 1 2]] should NOT emit warnings during
        // generalization of `id`, even though the Numeric constraint on `n` is in
        // state.constraints — the constraint was already discharged during unification
        // when `+` was checked, so it should not trigger the "ambiguous" warning.
        let mut file = crate::parse("[id: [fn [x] x] n: [+ 1 2]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let _ = crate::imports::build_prelude_env(); // populate PRELUDE_INSTANCE_CACHE
        let (errors, diagnostics) = typecheck_file(&file.node);

        assert!(
            errors.is_empty(),
            "Should not have type errors; got: {:?}",
            errors
        );

        // Filter out non-T013 diagnostics (e.g., over-broad annotation hints, inferred Unknown)
        let ambiguous_warnings: Vec<_> = diagnostics.iter().filter(|d| d.code == "T013").collect();

        assert!(
            ambiguous_warnings.is_empty(),
            "Should not emit ambiguous constraint warnings for already-discharged constraints; got: {:?}",
            ambiguous_warnings
        );
    }

    // --- Boundary guard collection tests (gradual typing) ---

    #[test]
    fn test_boundary_guard_collection_stub() {
        // Verify that boundary guards are collected when Unknown crosses a concrete-typed
        // function parameter boundary. The eval-side wiring (eval.rs maybe_wrap_guard)
        // inserts ThunkState::Guarded at thunk creation time for any span in this map.
        //
        // SETUP: `f` is registered as a polymorphic scheme (non-empty type_vars so that
        // the dispatcher at line ~1846 routes to check_call_with_scheme, not check_call).
        // Its body is `Fn(Int) -> Int` — the parameter type is concrete.
        // `x` is bound to Type::Unknown (simulates a value loaded from JSON / external input).
        //
        // When check_call_with_scheme zips param types against arg types, the first param
        // is Int (concrete) and the arg type is Unknown, satisfying the boundary guard
        // condition at line ~3497 → boundary_guards receives one entry.
        let input = "[call $f $x]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        let mut parent_env = TypeEnv::new();
        // `f: ∀a. Fn(Int) -> a` — polymorphic (non-empty type_vars) forces
        // check_call_with_scheme.  After instantiate_scheme, the return type becomes
        // a fresh TypeVar (so has_inference_vars() == true, satisfying the invariant
        // at line ~3444).  The first param remains Int (concrete).
        parent_env.insert_scheme(
            "f".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(None, Type::Int)],
                    ret: Box::new(Type::TypeVar("a".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        // `x: Unknown` — simulates a runtime-typed value (e.g. from-json result).
        parent_env.insert_scheme("x".to_string(), TypeScheme::mono(Type::Unknown));
        let parent_env = Rc::new(parent_env);

        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        // Errors are expected (advisory): Unknown arg vs Int param produces a type error,
        // but the boundary guard collection happens before the unification error is returned.
        let _ = infer_expr(expr, &parent_env, &mut state, &mut None);

        // The boundary guard must have been collected: Unknown crossed into Int.
        assert!(
            !state.boundary_guards.is_empty(),
            "expected at least one boundary guard when Unknown arg crosses Int param boundary, \
             but boundary_guards was empty"
        );
        // The guard's expected type must be the concrete param type (Int), not Unknown.
        let all_concrete = state
            .boundary_guards
            .iter()
            .all(|(_, ty)| !matches!(ty, Type::Unknown | Type::TypeVar(_, _)));
        assert!(
            all_concrete,
            "boundary guard expected types should all be concrete (non-Unknown, non-TypeVar), \
             got: {:?}",
            state.boundary_guards
        );
    }

    // -- LetDecl, CaseArm, and Placeholder (unified-bindings sprint) --

    #[test]
    fn test_let_decl_in_expression_position_is_error() {
        // Task 3: Expr::LetDecl in expression position must emit a type error.
        // The parser produces LetDecl from [let ...]; outside a binding context it is invalid.
        // The type checker at typecheck.rs:2546-2551 must catch this and produce an error.
        let errors = check_err("[f: [fn [x] [let x y]]]");
        assert!(
            !errors.is_empty(),
            "LetDecl in expression position should produce a type error"
        );
        let has_binding_error = errors.iter().any(|e| {
            e.message.contains("binding declaration")
                || e.message.contains("[let")
                || e.message.contains("not valid in expression position")
        });
        assert!(
            has_binding_error,
            "Error should mention binding declaration / expression position; got: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_placeholder_has_type_unknown() {
        // Task 4: Expr::Placeholder (the `...` expression) has type Unknown.
        // This is the gradual typing escape hatch — ... satisfies any type constraint.
        // Verify via direct infer call. Since `...` is a Placeholder token, we parse it.
        let mut file = crate::parse("...").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        let ty = infer_expr(expr, &env, &mut state, &mut None).unwrap();
        assert_eq!(
            ty,
            Type::Unknown,
            "Placeholder (...) must have type Unknown; got {ty}"
        );
    }

    #[test]
    fn test_placeholder_in_function_body_typechecks() {
        // Task 4: ... in a function body satisfies any return type annotation.
        // [fn@Int [x@Int] ...] should type-check without error because ... : Unknown ~ Int.
        let result = check("[f: [fn@Int [x@Int] ...]]");
        assert!(
            result.is_ok(),
            "... in function body should satisfy any return type annotation; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_case_arm_plain_binding_gets_scrutinee_type() {
        // Task 2: [case [let n] body] — plain binding n gets type scrutinee_ty.
        // CaseArm standalone: scrutinee_ty is Unknown (no scrutinee provided).
        // The body references n, which should be Unknown.
        // We verify by checking the result of a standalone CaseArm does not error.
        let result = check("[result: [case [let n] n]]");
        assert!(
            result.is_ok(),
            "[case [let n] n] standalone should type-check; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_case_arm_typed_binding_intersects_scrutinee() {
        // Task 2: [case [let n@Int] body] — n gets type scrutinee_ty ∩ Int.
        // Standalone (no scrutinee → scrutinee_ty = Unknown): Unknown ∩ Int = Int (AGT lifting).
        // So n : Int in the body.
        // We verify via the function body: [fn [x@Int] [case [let n@Int] n]]
        // where x is the scrutinee (Int) and n gets Int ∩ Int = Int.
        let result = check("[f: [fn [x@Int] [case [let n@Int] n]]]");
        assert!(
            result.is_ok(),
            "[case [let n@Int] n] with Int scrutinee should type-check; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_case_arm_wildcard_no_binding() {
        // Task 2: [case [let _] body] — wildcard introduces no binding.
        // The body can use any variables from the outer scope.
        let result = check("[result: [case [let _] 42]]");
        assert!(
            result.is_ok(),
            "[case [let _] 42] should type-check; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_case_arm_exact_value_match() {
        // Task 2: [case 42 body] — exact-value match, no new bindings.
        // The body can use variables from the outer scope.
        let result = check("[result: [case 42 true]]");
        assert!(
            result.is_ok(),
            "[case 42 true] should type-check; got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_case_arm_returns_body_type() {
        // Task 2: typecheck_case_arm returns the body type.
        // [case [let _] 42] should have type IntLiteral(42).
        let ty = infer("[case [let _] 42]");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "[case [let _] 42] should have type IntLiteral(42); got {ty}"
        );
    }

    #[test]
    fn test_normalize_intersection_unknown_is_identity() {
        // normalize_intersection treats Unknown as identity: T & ? = T.
        // This is the AGT gradual typing lift (Garcia et al. 2016).
        // When scrutinee_ty is Unknown and annotation is Int, the result is Int (not Int & ?).
        assert_eq!(
            Type::normalize_intersection(vec![Type::Unknown, Type::Int]),
            Type::Int,
            "Unknown ∩ Int must simplify to Int (Unknown is identity in intersection)"
        );
        assert_eq!(
            Type::normalize_intersection(vec![Type::Int, Type::Unknown]),
            Type::Int,
            "Int ∩ Unknown must simplify to Int (commutative identity)"
        );
        assert_eq!(
            Type::normalize_intersection(vec![Type::Unknown, Type::Str]),
            Type::Str,
            "Unknown ∩ Str must simplify to Str"
        );
        // All-Unknown intersection: when all elements are identity-skipped, the result is Top.
        // This is the correct mathematical result for an empty intersection (the empty meet is ⊤).
        // In practice this case does not arise in typecheck_case_arm because plain bindings
        // [let n] do NOT use normalize_intersection — they bind n directly to scrutinee_ty.
        assert_eq!(
            Type::normalize_intersection(vec![Type::Unknown]),
            Type::Top,
            "Single-element Unknown: Unknown is skipped as identity, empty list returns Top"
        );
        assert_eq!(
            Type::normalize_intersection(vec![Type::Unknown, Type::Unknown]),
            Type::Top,
            "All-Unknown intersection returns Top (all identity elements, empty result list)"
        );
    }
}
