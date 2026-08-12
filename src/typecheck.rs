//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::Arc;

use crate::type_tags::*;
// Pattern import deleted (T-1750)
use crate::ast::{Span, Spanned, SurfaceDocument, SurfaceExpression, SurfaceItem, SurfaceNode};
use crate::error::Diagnostic;
use crate::type_infer::InferenceContext;
use crate::types::{generalize_tv, InferState};
use crate::value::{Value, TYPE_ID_DICT};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
pub(crate) mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;
// Special-case type refinement dispatchers for polymorphic builtins
// Path-sensitive narrowing and overlap checking
#[path = "typecheck_narrow.rs"]
pub(crate) mod typecheck_narrow;
// Case arm and function literal type inference
#[path = "typecheck_match.rs"]
pub(crate) mod typecheck_match;
// Call and dot-access type checking
#[path = "typecheck_call.rs"]
pub(crate) mod typecheck_call;
// CEK machine for iterative type checking
#[path = "typecheck_cek.rs"]
pub(crate) mod typecheck_cek;

use typecheck_narrow::{
    extract_param_indices, extract_pattern_types, patterns_overlap, types_can_unify,
};

const KIND: &str = "type-check";

/// Returns the next globally unique class declaration ID.
/// Delegates to `type_class::next_class_decl_id` — the counter is defined there so the
/// resolver's Phase 1b scan and the type-checker share the same AtomicU64.
fn next_class_decl_id() -> u64 {
    crate::type_class::next_class_decl_id()
}

/// Map from source span `(start_line, start_col, end_line, end_col)` to inferred TypeValue.
/// Populated during type checking so LSP hover/diagnostics can look up types without
/// re-running inference.
pub type TypeMap = HashMap<(u32, u32, u32, u32), Arc<Value>>;

// MatchArmData (old Expr-based) removed — replaced by SurfaceMatchArmData.

/// Grouped surface data for a `[class ...]` declaration.
///
/// Passed to [`infer_class_decl_from_surface`] instead of individual arguments.
pub(crate) struct ClassDeclSurface<'a> {
    pub name: &'a str,
    pub params: &'a [String],
    pub superclasses: &'a [(String, Vec<String>)],
    pub determines: &'a [Arc<SurfaceNode>],
    pub structural: &'a str,
    pub span: Span,
    /// Optional resolver name (from `resolver:` in class metadata). Stored in ClassDecl.
    pub resolver: Option<String>,
    /// Whether the resolver is injective (from `resolver_injective:` in class metadata).
    pub resolver_injective: bool,
    /// Pre-assigned class declaration ID from the resolver's Phase 1b scan.
    /// When `Some`, the type-checker reads this ID instead of allocating a new one,
    /// ensuring the EffectPerform dispatch chain uses a consistent class_decl_id.
    /// `None` means the resolver did not run Phase 1b (e.g. bootstrap typecheck path).
    pub pre_assigned_class_decl_id: Option<&'a crate::ast::ClassDeclIdCell>,
}

/// Type-check a single [`SurfaceDocument`] using the CEK machine.
///
/// Routes all document items through the CEK's Sequential arm via `run_typecheck` — the single
/// canonical path for sequential expression evaluation. For single-item documents, the node
/// is evaluated directly (no Sequential wrapper).
///
/// After `run_typecheck`, ctor_schemes (ADT constructor TypeSchemes) are recovered from the
/// diff of `state.tycon_env` against a pre-run snapshot: `typecheck_letrec_group` does not return
/// ctor_schemes for the terminal dict (the LetrecGroupPassZero caller discards the schemes return),
/// so we reconstruct them here for result_env.
///
/// # Returns
///
/// `(result_env, result_type, errors)` where:
/// - `result_env`: env containing schemes from the last dict body, exported to subsequent documents
/// - `result_type`: the type of the last expression (or empty-dict for empty documents)
/// - `errors`: type errors encountered during inference (non-fatal — env always propagated)
/// Extract fields from a TypeValue.Record as `Vec<(field_name, TypeValue)>`.
/// Public alias for use by typecheck_narrow.rs.
pub(crate) fn extract_record_fields_pub(tv: &Arc<Value>) -> Option<Vec<(String, Arc<Value>)>> {
    extract_record_fields(tv)
}
///
/// Returns `Some(fields)` for TypeValue.Record variants where the payload can be read
/// synchronously (settled thunks). Returns `None` for non-Record TypeValues or unsettled payloads.
fn extract_record_fields(tv: &Arc<Value>) -> Option<Vec<(String, Arc<Value>)>> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            ctor_index,
            payload: Some(payload_thunk),
            ..
        } if *ctor_index == TV_RECORD => match payload_thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                let fields_thunk = entries.get(&fields_key)?;
                match fields_thunk.peek_result()? {
                    Ok(Value::Dict {
                        entries: field_entries,
                        ..
                    }) => {
                        let mut result = Vec::new();
                        for (k, v_thunk) in field_entries {
                            if let HashableValue::Str(name) = k {
                                if let Some(Ok(field_val)) = v_thunk.peek_result() {
                                    result.push((
                                        name.as_ref().to_string(),
                                        Arc::new(field_val.clone()),
                                    ));
                                }
                            }
                        }
                        Some(result)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Public alias for use by typecheck_narrow.rs.
pub(crate) fn make_typevalue_record_pub(
    fields: indexmap::IndexMap<String, Arc<Value>>,
    tail: Option<Arc<Value>>,
) -> Arc<Value> {
    make_typevalue_record(fields, tail)
}

/// Construct a TypeValue.Record Arc<Value> from an IndexMap of field name → TypeValue pairs
/// and an optional tail TypeValue. `tail: None` means closed record (RowTail.Closed).
fn make_typevalue_record(
    fields: indexmap::IndexMap<String, Arc<Value>>,
    tail: Option<Arc<Value>>,
) -> Arc<Value> {
    use crate::value::{HashableValue, Thunk};
    // Build the fields dict: { fieldname: TypeValue, ... }
    let mut fields_entries: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
        indexmap::IndexMap::new();
    for (name, tv) in fields {
        fields_entries.insert(
            HashableValue::Str(Arc::from(name.as_str())),
            Arc::new(Thunk::value(Value::clone(tv.as_ref()), crate::rust_span!())),
        );
    }
    let fields_dict_val = Value::Dict {
        entries: fields_entries,
        type_decl_id: TYPE_ID_DICT,
        type_val: None,
    };
    // tail: None → closed record → RowTail.Closed variant
    let tail_val = tail
        .map(|tv| Value::clone(tv.as_ref()))
        .unwrap_or_else(|| crate::type_infer::make_rowtail_closed().as_ref().clone());
    // Build payload dict: { fields: Dict, tail: TypeValue }
    let mut payload_entries: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
        indexmap::IndexMap::new();
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_FIELDS)),
        Arc::new(Thunk::value(fields_dict_val, crate::rust_span!())),
    );
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_TAIL)),
        Arc::new(Thunk::value(tail_val, crate::rust_span!())),
    );
    let payload_dict = Value::Dict {
        entries: payload_entries,
        type_decl_id: TYPE_ID_DICT,
        type_val: None,
    };
    Arc::new(Value::Variant {
        type_decl_id: crate::value::TYPE_ID_TYPE_VALUE,
        ctor_index: TV_RECORD,
        payload: Some(Arc::new(Thunk::value(payload_dict, crate::rust_span!()))),
        type_val: None,
    })
}

pub(crate) async fn process_document(
    doc: &SurfaceDocument,
    state: &mut InferState,
    scope: &crate::symbol_table::ScopeChain,
    type_map: &mut Option<&mut TypeMap>,
) -> (Arc<Value>, Vec<Diagnostic>) {
    state.current_scope = scope.clone();

    // Empty record TypeValue: TypeValue.Record { fields: {}, tail: [] (closed) }
    let empty_dict_ty = make_typevalue_record(indexmap::IndexMap::new(), None);

    // Collect all items in source order as SurfaceNodes.
    // SurfaceItem::Expr → use the node directly.
    // SurfaceItem::Decl → synthetic node with SurfaceExpression::Decl so infer_step::Decl
    //   handles class/instance/type-decl registration (TypeAlias deleted in S-1003).
    let nodes: Vec<Arc<SurfaceNode>> = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node) => Arc::clone(node),
            SurfaceItem::Decl(d) => Arc::new(SurfaceNode::new(
                SurfaceExpression::Decl(Box::new(d.node.clone())),
                d.span.clone(),
            )),
        })
        .collect();

    if nodes.is_empty() {
        return (empty_dict_ty, Vec::new());
    }

    let _enclosing_level = state.ctx.current_level;
    let last_span = nodes.last().unwrap().span.clone();

    // Route all document nodes through the CEK machine's Sequential arm — the single
    // canonical path for all expression evaluation. Every document is a Sequential,
    // including single-expression documents (degenerate case).
    let span = nodes[0].span.clone();
    let doc_node = Arc::new(SurfaceNode::new(SurfaceExpression::Sequential(nodes), span));

    let mut errors = Vec::new();
    let result_ty =
        typecheck_cek::run_typecheck(&doc_node, state, &mut errors, type_map, &mut Vec::new())
            .await;

    // Check that the last expression's type is a Dict subtype.
    {
        let is_dict_ok = match result_ty.as_ref() {
            Value::Variant { ctor_index, .. } => {
                let c = *ctor_index;
                c == TV_RECORD
                    || c == TV_UNKNOWN
                    || c == TV_TOP
                    || c == TV_VAR
                    || c == TV_NOMINAL
                    || c == TV_APP
            }
            // Bootstrap sentinel (empty dict = Unknown during bootstrap)
            Value::Dict { entries, .. } if entries.is_empty() => true,
            _ => false,
        };
        if !is_dict_ok {
            errors.push(Diagnostic::error(
                KIND,
                format!(
                    "document last expression must be a record type, got {:?}",
                    result_ty
                ),
                last_span,
            ));
        }
    }

    (result_ty, errors)
}

/// Pre-register type aliases in `target_env` before letrec inference.
///
/// Map a resolved TypeValue (Arc<Value>) to the dispatch tag string used in instance dispatch.
///
/// This mapping must match the dispatch tag extraction logic, which reads `@Annotation`
/// names from instance arm patterns.  Annotations are written as `@Integer`, `@Float`,
/// `@String`, `@Bytes`, `@SomeType`.
///
/// Returns `None` for:
/// - Unbound `TypeValue.Var` (instance not yet determined).
/// - `TypeValue.Unknown` / `TypeValue.Top` (gradual / lattice types that don't correspond to instances).
/// - Compound types that don't map to a single dispatch tag (records, functions, unions).
///
/// Literal types (IntLit, FloatLit, StrLit) are promoted to their widened tags
/// (`"Integer"`, `"Float"`, `"String"`) because instance arms are always annotated
/// with the widened type (e.g., `@Integer`, never `@42`).
pub(crate) fn type_to_dispatch_tag(tv: &Arc<Value>) -> Option<String> {
    match tv.as_ref() {
        Value::Variant { ctor_index, .. } => match *ctor_index {
            TV_NOMINAL => {
                let id = crate::type_infer::typevalue_nominal_id(tv)?;
                match id {
                    crate::value::TYPE_ID_INT | crate::value::TYPE_ID_U64 => {
                        Some(DISPATCH_INTEGER.to_string())
                    }
                    crate::value::TYPE_ID_FLOAT => Some(DISPATCH_FLOAT.to_string()),
                    crate::value::TYPE_ID_STRING => Some(DISPATCH_STRING.to_string()),
                    crate::value::TYPE_ID_BYTES => Some(DISPATCH_BYTES.to_string()),
                    _ => None,
                }
            }
            TV_INT_LIT => Some(DISPATCH_INTEGER.to_string()),
            TV_FLOAT_LIT => Some(DISPATCH_FLOAT.to_string()),
            TV_STR_LIT => Some(DISPATCH_STRING.to_string()),
            // Inference variables and gradual types cannot be dispatched.
            TV_VAR | TV_UNKNOWN | TV_TOP | TV_NEVER => None,
            // Compound types (Record, Fn, Union, App, etc.) don't map to single dispatch tags.
            _ => None,
        },
        // Bootstrap sentinel (empty dict = Unknown) → not dispatchable.
        _ => None,
    }
}

/// Extract the `name` string from a TypeValue.Op variant payload (synchronously).
/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::typecheck_letrec_group — no Expr bridge needed.
pub(crate) fn infer_class_decl_from_surface(
    decl: &ClassDeclSurface<'_>,
    state: &mut InferState,
) -> Result<Arc<Value>, Vec<Diagnostic>> {
    use crate::types::ClassDecl;
    // After S-1003: Kind deleted, params use Arc<Value> TypeValue kind.

    let ClassDeclSurface {
        name,
        params,
        superclasses,
        determines,
        structural,
        span,
        resolver,
        resolver_injective,
        pre_assigned_class_decl_id,
    } = decl;
    let span = span.clone();

    if name.is_empty() {
        return Err(vec![Diagnostic::error(
            KIND,
            "class declaration must have a name declared with [class [ClassName ...] ...]",
            span,
        )]);
    }

    // Method body validation is handled by dict inference (type declaration registration step, typecheck_dict.rs).
    // resolve_type_expr is async and cannot be called from sync infer_class_decl_from_surface.
    // method_signatures is populated as empty (matches existing ClassDecl construction sites).

    // After S-1003: ClassDecl.params is Vec<(String, Arc<Value>)> where Arc<Value> is the kind TypeValue.
    let existing_param_kinds: std::collections::HashMap<String, Arc<Value>> = {
        match state.classes.iter().find(|c| c.name == *name) {
            Some(existing) => existing.params.iter().cloned().collect(),
            None => std::collections::HashMap::new(), // Class not yet registered, treat as zero params.
        }
    };

    let mut fd_indices: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for fd_node in *determines {
        match &fd_node.expr {
            SurfaceExpression::Dict { entries, scope: _ } if entries.len() == 2 => {
                let determining = &entries[0].node.value;
                let determining_indices =
                    extract_param_indices(determining, params, fd_node.span.clone())?;
                let determined = &entries[1].node.value;
                let determined_indices =
                    extract_param_indices(determined, params, fd_node.span.clone())?;
                fd_indices.push((determining_indices, determined_indices));
            }
            _ => {
                return Err(vec![Diagnostic::error(KIND,
                    "functional dependency must be a 2-element list [[determining-vars] determined-var(s)]",
                    fd_node.span.clone(),
                )]);
            }
        }
    }

    let structural_discharge = match *structural {
        STRUCTURAL_CLOSED_DICT => crate::type_class::StructuralDischarge::ClosedDict,
        _ => crate::type_class::StructuralDischarge::None,
    };

    // Use the pre-assigned class_decl_id if the resolver's Phase 1b scan set one.
    // This ensures the EffectPerform dispatch chain uses the same ID that was written
    // into VarAddr::EffectPerform during resolution. Fall back to a fresh ID for
    // the bootstrap typecheck path (where Phase 1b does not run).
    let class_decl_id = pre_assigned_class_decl_id
        .and_then(|cell| cell.get())
        .unwrap_or_else(next_class_decl_id);

    let class_decl = ClassDecl {
        class_decl_id,
        name: name.to_string(),
        params: params
            .iter()
            .map(|p| {
                // After S-1003: default kind is TypeValue.Op{name:"Type"} (kind *).
                let kind = existing_param_kinds
                    .get(p)
                    .cloned()
                    .unwrap_or_else(|| crate::type_infer::make_typevalue_nominal(0));
                (p.clone(), kind)
            })
            .collect(),
        superclasses: superclasses
            .iter()
            .map(|(class_name, params)| (class_name.clone(), params.clone()))
            .collect(),
        determines: fd_indices,
        resolver: resolver.clone(),
        resolver_injective: *resolver_injective,
        structural_discharge,
        method_signatures: vec![],
    };

    state.classes.push(class_decl.clone());
    // invalidate_env_caches removed in S-1046 Phase 2 (no env-level caches anymore)
    // Class entries are stored in state.classes (flat registry).
    // No insertion into type_stage_scope needed.
    // After S-1003: Kind deleted, kind_env removed from InferState.
    // Operator-kinded params are now tracked as TypeValue kinds in ClassDecl.params.
    // No further action needed — the kind information is already in class_decl.params.

    // Populate non_injective_resolvers in the InferenceContext so that unify() can look up
    // injectivity for TV_APP~TV_APP without requiring access to the full Env.
    if !class_decl.resolver_injective {
        if let Some(ref resolver_name) = class_decl.resolver {
            if let Some(&id) = state.type_name_to_id.get(resolver_name.as_str()) {
                state.ctx.non_injective_resolvers.insert(id);
            }
        }
    }

    Ok(make_typevalue_record(indexmap::IndexMap::new(), None))
}

/// Type alias for match arm type data (Surface version): (param_types, span, entries).
type SurfaceMatchArmData<'a> = (
    Vec<Arc<Value>>,
    Span,
    &'a Vec<Spanned<crate::ast::SurfaceEntry>>,
);

/// Type-check an [instance ...] declaration from SurfaceDeclaration::InstanceDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::typecheck_letrec_group — no Expr bridge needed.
pub(crate) async fn infer_instance_decl_from_surface(
    class_name: &str,
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<crate::ast::SurfaceEntry>>)],
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Arc<Value>, Vec<Diagnostic>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(make_typevalue_record(indexmap::IndexMap::new(), None));
    }

    let (param_count, has_fds, fd_list, param_names) = {
        let class_decl = state
            .classes
            .iter()
            .find(|c| c.name == class_name)
            .cloned()
            .ok_or_else(|| {
                vec![Diagnostic::error(
                    KIND,
                    format!("unknown class '{}'", class_name),
                    span.clone(),
                )]
            })?;
        (
            class_decl.params.len(),
            !class_decl.determines.is_empty(),
            class_decl.determines.clone(),
            class_decl
                .params
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>(),
        )
    };

    let mut arm_data: Vec<SurfaceMatchArmData> = Vec::new();

    for (pattern_node, methods) in arms {
        let pattern_types = extract_pattern_types(pattern_node, state).await?;

        if pattern_types.len() != param_count {
            return Err(vec![Diagnostic::error(
                KIND,
                format!(
                    "instance pattern has {} type parameters but class '{}' expects {}",
                    pattern_types.len(),
                    class_name,
                    param_count
                ),
                pattern_node.span.clone(),
            )]);
        }

        // Allow TV_VAR positions (declared TypeVars from bind: in [let ...]).
        // Reject only Unknown (empty dict or TV_UNKNOWN): those indicate missing annotations.
        if pattern_types.iter().any(|tv| {
            matches!(tv.as_ref(), Value::Variant { ctor_index, .. } if *ctor_index == TV_UNKNOWN)
                || matches!(tv.as_ref(), Value::Dict { entries, .. } if entries.is_empty())
        }) {
            return Err(vec![Diagnostic::error(KIND,
                format!(
                    "instance pattern for class '{}' contains Unknown types — all pattern positions must have concrete type annotations (use a@Type syntax) or TypeVars declared via bind: in [let ...]",
                    class_name
                ),
                pattern_node.span.clone(),
            )]);
        }

        arm_data.push((pattern_types, pattern_node.span.clone(), methods));
    }

    for i in 0..arm_data.len() {
        for j in (i + 1)..arm_data.len() {
            let (types_i, span_i, _) = &arm_data[i];
            let (types_j, span_j, _) = &arm_data[j];

            if patterns_overlap(types_i, types_j, state).await? {
                let error = Diagnostic::error(KIND,
                    format!(
                        "overlapping instance patterns for class '{}': arm at line {} and arm at line {} could both match the same types",
                        class_name,
                        span_i.start_line,
                        span_j.start_line
                    ),
                    span_j.clone(),
                );
                return Err(vec![error]);
            }
        }
    }

    if has_fds {
        for (determining_indices, determined_indices) in &fd_list {
            for i in 0..arm_data.len() {
                for j in (i + 1)..arm_data.len() {
                    let (types_i, span_i, _) = &arm_data[i];
                    let (types_j, span_j, _) = &arm_data[j];

                    let determining_i: Vec<Arc<Value>> = determining_indices
                        .iter()
                        .map(|&idx| Arc::clone(&types_i[idx]))
                        .collect();
                    let determining_j: Vec<Arc<Value>> = determining_indices
                        .iter()
                        .map(|&idx| Arc::clone(&types_j[idx]))
                        .collect();

                    if types_can_unify(&determining_i, &determining_j, state).await? {
                        let determined_i: Vec<Arc<Value>> = determined_indices
                            .iter()
                            .map(|&idx| Arc::clone(&types_i[idx]))
                            .collect();
                        let determined_j: Vec<Arc<Value>> = determined_indices
                            .iter()
                            .map(|&idx| Arc::clone(&types_j[idx]))
                            .collect();

                        if !types_can_unify(&determined_i, &determined_j, state).await? {
                            let error = Diagnostic::error(KIND,
                                format!(
                                    "consistency violation for class '{}': arm at line {} and arm at line {} have overlapping determining positions but incompatible determined types",
                                    class_name,
                                    span_i.start_line,
                                    span_j.start_line
                                ),
                                span_j.clone(),
                            );
                            return Err(vec![error]);
                        }
                    }
                }
            }
        }
    }

    for (pattern_types, _arm_span, methods) in &arm_data {
        let inst_type = if pattern_types.len() == 1 {
            Arc::clone(&pattern_types[0])
        } else {
            let fields: indexmap::IndexMap<String, Arc<Value>> = pattern_types
                .iter()
                .enumerate()
                .map(|(i, tv)| (i.to_string(), Arc::clone(tv)))
                .collect();
            make_typevalue_record(fields, None)
        };

        // Extract type tags for instance binding name generation.
        // Only concrete uppercase type names contribute to the binding name — TypeVars and
        // Unknown are filtered out.
        let type_args: Vec<String> = pattern_types
            .iter()
            .filter_map(type_to_dispatch_tag)
            .collect();

        let mut method_types = HashMap::new();

        // Inject class type parameter bindings into type_stage_scope so that
        // method body annotations referencing the class's type parameters resolve to the
        // concrete pattern types for this arm.
        //
        // For example: class `Equatable [let a]` with method `=: [Fn@Boolean [a a]]` and
        // arm pattern `[let a@Integer]` (pattern_types = [Type::Int]) → inject `a → Int`.
        // Any annotation `@a` inside the method body now resolves to `Type::Int` instead
        // of the catch-all behavior. For the arm `[pattern [Int]]` where the pattern type
        // is already resolved to `Type::Int` (via the zero-arg Call fix in extract_binding_types),
        // this also injects the correct concrete type.
        //
        // Implementation: push a new innermost type_stage_scope frame (index 0) containing
        // the param_name → TypeValue (pattern_type) bindings. Pop it after all
        // methods in this arm are checked. This is a temporary scope — it does NOT leak into
        // other arms or the surrounding type environment.
        let param_type_frame: std::collections::HashMap<String, crate::type_infer::TypeValue> = {
            let mut frame = std::collections::HashMap::new();
            for (name, tv) in param_names.iter().zip(pattern_types.iter()) {
                // Skip TypeValue.Var — unresolved type variable, not a concrete pattern type.
                let is_var = matches!(tv.as_ref(), Value::Variant { ctor_index, .. } if ctor_index == &TV_VAR);
                if !is_var {
                    frame.insert(name.clone(), Arc::clone(tv));
                }
            }
            frame
        };
        let pushed_frame = !param_type_frame.is_empty();
        if pushed_frame {
            state.type_stage_data.scope.push(param_type_frame.clone());
        }

        for method in *methods {
            let method_name = match &method.node.key {
                Some(key_node) => match &key_node.expr {
                    SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                    SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                    _ => {
                        if pushed_frame {
                            state.type_stage_data.scope.remove(0);
                        }
                        return Err(vec![Diagnostic::error(
                            KIND,
                            "instance method name must be a string or identifier",
                            key_node.span.clone(),
                        )]);
                    }
                },
                None => {
                    if pushed_frame {
                        state.type_stage_data.scope.remove(0);
                    }
                    return Err(vec![Diagnostic::error(
                        KIND,
                        "instance method must have a name",
                        method.span.clone(),
                    )]);
                }
            };

            // Bidirectional type checking — look up the class method's polymorphic
            // signature, instantiate it with the concrete pattern types, and set
            // state.expected_fn_params so infer_fn_push_cont can type unannotated params.
            //
            // Example: class Equatable [let a] with signature eq: [Fn@Boolean [a a]].
            // For instance [let a@Int], method_sig = Fn(a,a)→Bool → instantiate a→Int
            // → Fn(Int,Int)→Bool → expected_fn_params = [Int, Int].
            // Unannotated params x, y in `[fn [let x y] ...]` then get Type::Int instead
            // of Type::Unknown.
            //
            // ClassDecl.method_signatures is populated by the CEK pass when the [class ...]
            // declaration is processed. By the time [instance ...] arms are checked, the
            // class body has already been type-checked, so method_signatures is available.
            {
                // Build a TypeValue substitution from the param_type_frame, mapping class
                // param names (e.g., "a") to the concrete pattern TypeValues (e.g., Int).
                // param_type_frame contains TypeValue directly for each concrete param.
                let type_subst: HashMap<String, Arc<Value>> = param_type_frame
                    .iter()
                    .map(|(name, tv)| (name.clone(), Arc::clone(tv)))
                    .collect();

                // Look up this method's polymorphic signature in the class.
                let method_sig_opt = state
                    .classes
                    .iter()
                    .find(|c| c.name == class_name)
                    .cloned()
                    .and_then(|cd| {
                        cd.method_signatures
                            .iter()
                            .find(|(n, _)| n == &method_name)
                            .map(|(_, tv)| Arc::clone(tv))
                    });

                if let Some(poly_sig) = method_sig_opt {
                    // Apply the substitution to the polymorphic method type to get the
                    // specialized (monomorphic) signature for this instance arm.
                    let concrete_sig =
                        crate::types::apply_typevalue_renaming(&poly_sig, &type_subst);

                    // Extract the param types from the concrete TypeValue.Fn.
                    if let Some((param_types, _ret)) =
                        crate::type_infer::typevalue_fn_params_and_ret(&concrete_sig)
                    {
                        if !param_types.is_empty() {
                            state.expected_fn_params = Some(param_types);
                        }
                    }
                }
                // If method_signatures is empty (class body not yet processed or no signature),
                // or if the method is not found, skip — unannotated params fall back to Unknown.
            }

            let mut method_errors: Vec<Diagnostic> = Vec::new();
            let mut method_stack = Vec::new();
            let method_impl_type = Box::pin(typecheck_cek::run_typecheck(
                &method.node.value,
                state,
                &mut method_errors,
                type_map,
                &mut method_stack,
            ))
            .await;
            // Clear expected_fn_params in case run_typecheck didn't consume it
            // (e.g. the method body is not a fn expression).
            state.expected_fn_params = None;
            if !method_errors.is_empty() {
                if pushed_frame {
                    state.type_stage_data.scope.remove(0);
                }
                return Err(method_errors);
            }

            // T-2145 Feature 1: Validate instance method params against class declaration signature.
            //
            // For each instance method, check that its parameter types are compatible with
            // the class method signature (after substituting class type parameters with the
            // instance's concrete pattern types). Emit a warning if they don't unify.
            //
            // This is a TYPECHECK-TIME validation — the runtime dispatch handles correctness,
            // but catching param mismatches here improves the developer experience.
            {
                // Look up the class method signature.
                let class_method_sig_opt = state
                    .classes
                    .iter()
                    .find(|c| c.name == class_name)
                    .cloned()
                    .and_then(|cd| {
                        cd.method_signatures
                            .iter()
                            .find(|(n, _)| n == &method_name)
                            .map(|(_, tv)| Arc::clone(tv))
                    });

                if let Some(poly_sig) = class_method_sig_opt {
                    // Substitute class params with the concrete instance pattern types.
                    let type_subst: HashMap<String, Arc<Value>> = param_type_frame
                        .iter()
                        .map(|(name, tv)| (name.clone(), Arc::clone(tv)))
                        .collect();
                    let concrete_class_sig =
                        crate::types::apply_typevalue_renaming(&poly_sig, &type_subst);

                    // Extract param types from both the class signature and the instance method.
                    let class_params_opt =
                        crate::type_infer::typevalue_fn_params_and_ret(&concrete_class_sig);
                    let method_params_opt =
                        crate::type_infer::typevalue_fn_params_and_ret(&method_impl_type);

                    match (class_params_opt, method_params_opt) {
                        (Some((class_params, _class_ret)), Some((method_params, _method_ret))) => {
                            // Check that the instance method has the same arity as the class signature.
                            if class_params.len() != method_params.len() {
                                state.diagnostics.push(Diagnostic::warn(
                                    KIND,
                                    format!(
                                        "instance method '{}' for class '{}' has {} parameters, but class signature expects {}",
                                        method_name,
                                        class_name,
                                        method_params.len(),
                                        class_params.len()
                                    ),
                                    method.span.clone(),
                                ));
                            } else {
                                // Check that each param type unifies with the class signature's param type.
                                // Use a fresh InferenceContext snapshot to avoid polluting the global state.
                                let mut validation_ctx = InferenceContext::from_snapshot(
                                    state.ctx.subst.clone(),
                                    state.ctx.levels.clone(),
                                    state.ctx.current_level,
                                    state.ctx.tycon_env.clone(),
                                    state.type_name_to_id.clone(),
                                );
                                let mut validation_constraints = Vec::new();

                                for (i, (class_param, method_param)) in
                                    class_params.iter().zip(method_params.iter()).enumerate()
                                {
                                    let unify_result = Box::pin(crate::types::unify(
                                        class_param,
                                        method_param,
                                        &mut validation_ctx,
                                        &mut validation_constraints,
                                        method.span.clone(),
                                        0,
                                    ))
                                    .await;

                                    if let Err(err) = unify_result {
                                        state.diagnostics.push(
                                            Diagnostic::warn(
                                                KIND,
                                                format!(
                                                    "instance method '{}' parameter {} has type incompatible with class signature",
                                                    method_name, i
                                                ),
                                                method.span.clone(),
                                            )
                                            .with_note(format!(
                                                "class signature expects: {}",
                                                crate::eval::format_type_with_names(
                                                    class_param, &state.ctx.type_display_names
                                                )
                                            ))
                                            .with_note(format!(
                                                "instance method has: {}",
                                                crate::eval::format_type_with_names(
                                                    method_param, &state.ctx.type_display_names
                                                )
                                            ))
                                            .with_note(format!("unification failed: {}", err.message)),
                                        );
                                    }
                                }
                            }
                        }
                        (None, Some(_)) => {
                            // Instance method is a function, but class signature is not.
                            state.diagnostics.push(Diagnostic::warn(
                                KIND,
                                format!(
                                    "instance method '{}' for class '{}' is a function, but class signature is not",
                                    method_name, class_name
                                ),
                                method.span.clone(),
                            ));
                        }
                        (Some(_), None) => {
                            // Class signature is a function, but instance method is not.
                            state.diagnostics.push(Diagnostic::warn(
                                KIND,
                                format!(
                                    "instance method '{}' for class '{}' is not a function, but class signature expects one",
                                    method_name, class_name
                                ),
                                method.span.clone(),
                            ));
                        }
                        (None, None) => {
                            // Neither is a function — this is OK (methods can be non-function values).
                            // No validation needed.
                        }
                    }
                }
                // If class_method_sig is None, the class has no signature for this method —
                // this is OK (the method might be added dynamically). No validation needed.
            }

            method_types.insert(method_name.clone(), method_impl_type.clone());

            // Insert TypeValue for the method so that VarRef resolution
            // can find the method type.
            let binding_name = method_name.clone();

            let _scheme = generalize_tv(state.ctx.current_level, &method_impl_type, &state.ctx);

            // Type scheme write removed — current_scope (SymbolTable) on InferState handles type scheme storage.
            let _ = binding_name;
        }

        // Pop the type param scope frame pushed before method body checking.
        if pushed_frame {
            state.type_stage_data.scope.remove(0);
        }

        let det_positions: Vec<usize> = {
            let mut seen = std::collections::HashSet::new();
            let mut positions = Vec::new();
            for (det_indices, _) in &fd_list {
                for &idx in det_indices {
                    if seen.insert(idx) {
                        positions.push(idx);
                    }
                }
            }
            positions.sort_unstable();
            positions
        };

        let instance_decl = InstanceDecl {
            class_name: class_name.to_string(),
            instance_type: inst_type,
            det_positions,
            method_types,
        };

        // Structural overlap is async (calls unify) and cannot be checked from this sync function.
        // Exact duplicates are caught by the mangled-key dedup in env.insert_instance.
        // After S-1003: instance_type is Arc<Value>. Use the dispatch tag strings for mangling.
        // type_args contains the dispatch tag strings derived from pattern_types.
        let type_args_for_mangling: Vec<&str> = type_args.iter().map(String::as_str).collect();
        let mangled = format!(
            "ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽",
            instance_decl.class_name,
            type_args_for_mangling.join(",")
        );
        state.instances.insert(mangled, instance_decl);
        state.invalidate_env_caches();
    }

    Ok(make_typevalue_record(indexmap::IndexMap::new(), None))
}

// contains_unknown_or_top removed — check_surface_expr now uses constrain() which handles
// gradual types internally (Unknown absorption in unify). The helper is no longer needed.

#[cfg(test)]
#[path = "typecheck_tests.rs"]
mod tests;
