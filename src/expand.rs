//! Declaration expansion — pre-resolve AST transformation.
//!
//! This module expands declaration nodes inside dict entries into equivalent
//! `SurfaceExpression` trees:
//!
//! - **TypeAlias** (T-2216) — expanded to TypeDecl/ReprDecl wrapping UnitVariant/MakeVariant
//! - **InstanceDecl** (T-2217) — fanned out into individual method entries with type
//!   guard annotations injected from the arm pattern
//! - **ClassDecl** (T-2218) — replaced with an empty dict placeholder
//! - **Spread-dict** (T-2218) — `[a: 1  ...rest  b: 2]` rewritten to nested
//!   `Call(VarRef("builtin-dict-merge"), ...)` at the SurfaceExpression level
//!
//! The expand stage runs after parsing and desugaring, before resolution. It is
//! a purely structural AST-to-AST transformation — no evaluation, no name resolution,
//! no type checking.
//!
//! ## Pipeline position
//!
//! ```text
//! parse → desugar → expand → resolve → typecheck → lower → eval
//! ```
//!
//! ## Walker pattern
//!
//! Follows the same recursive traversal as `src/desugar.rs`:
//! - `expand_program` → `expand_document` → per-item expansion
//! - `expand_node` recursively walks `Arc<SurfaceNode>` using `Arc::make_mut`
//! - `expand_dict_entries` handles dict-level expansions (TypeAlias, InstanceDecl, ClassDecl)
//! - Spread-dict expansion is checked first in the Dict arm of `expand_node`

use crate::ast::{
    Annotation, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceParam, SurfaceProgram,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Type declaration ID counter
// ---------------------------------------------------------------------------

/// Global counter for generating unique type declaration IDs.
///
/// Each `[type ...]` declaration gets a unique ID threaded through `TypeDecl`,
/// `UnitVariant`, and `MakeVariant` nodes so the evaluator can associate variants
/// with their parent type's identity in `type_identity_registry` — even when
/// multiple types share the same name in different scopes (B-714).
///
/// Starts at `TYPE_ID_RESERVED_MAX + 1` (100) to avoid collisions with primitive
/// type IDs (1..=99).
static TYPE_DECL_ID_COUNTER: AtomicU64 = AtomicU64::new(crate::value::TYPE_ID_RESERVED_MAX + 1);

/// Returns the next globally unique type declaration ID.
/// User-defined variants get IDs starting at 100; primitive Value variants use IDs 1..=99.
fn next_type_decl_id() -> u64 {
    TYPE_DECL_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Walker: program → document → node
// ---------------------------------------------------------------------------

/// Expand all declarations in a `SurfaceProgram`.
///
/// Returns a new program with all `TypeAlias` declarations inside dict entries
/// replaced by their expanded `SurfaceExpression` equivalents.
pub fn expand_program(program: &SurfaceProgram) -> SurfaceProgram {
    let documents = program
        .documents
        .iter()
        .map(|doc_spanned| {
            let new_doc = expand_document(&doc_spanned.node);
            Spanned::new(Arc::new(new_doc), doc_spanned.span.clone())
        })
        .collect();
    SurfaceProgram { documents }
}

/// Expand declarations in a single `SurfaceDocument`.
fn expand_document(doc: &SurfaceDocument) -> SurfaceDocument {
    let items = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node_arc) => {
                let mut new_node = Arc::clone(node_arc);
                expand_node(&mut new_node);
                SurfaceItem::Expr(new_node)
            }
            // SurfaceItem::Decl — top-level declarations are not expanded here.
            // Only Decl(TypeAlias) embedded inside dict entry values are expanded.
            other => other.clone(),
        })
        .collect();
    SurfaceDocument {
        header: doc.header.clone(),
        items,
    }
}

/// Recursively walk a `SurfaceNode`, expanding declarations in place.
///
/// Uses `Arc::make_mut` for copy-on-write — unmodified subtrees share their
/// original `Arc` with zero copying.
fn expand_node(node: &mut Arc<SurfaceNode>) {
    let node_mut = Arc::make_mut(node);

    match &mut node_mut.expr {
        // Leaf nodes: no children to recurse into.
        SurfaceExpression::Int(_)
        | SurfaceExpression::U64(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::VarRef {
            annotation: None, ..
        }
        | SurfaceExpression::Placeholder(..)
        | SurfaceExpression::Error(_)
        | SurfaceExpression::UnitVariant { .. } => {}

        // Decl nodes in standalone expression position (not inside a dict entry).
        // TypeAlias → empty dict (constructors only meaningful when bound to a name).
        // InstanceDecl → dict of fanned-out method entries.
        // ClassDecl → empty dict placeholder.
        SurfaceExpression::Decl(_) => {
            // Clone decl data before reassigning node_mut.expr to avoid borrow conflict.
            enum DeclAction {
                EmptyDict,
                InstanceFanOut(Vec<(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)>, Span),
            }
            let action = match &node_mut.expr {
                SurfaceExpression::Decl(decl) => match decl.as_ref() {
                    SurfaceDeclaration::TypeAlias { .. } => Some(DeclAction::EmptyDict),
                    SurfaceDeclaration::InstanceDecl { arms, .. } => Some(
                        DeclAction::InstanceFanOut(arms.clone(), node_mut.span.clone()),
                    ),
                    SurfaceDeclaration::ClassDecl { .. } => Some(DeclAction::EmptyDict),
                    _ => None,
                },
                _ => None,
            };
            match action {
                Some(DeclAction::EmptyDict) => {
                    node_mut.expr = SurfaceExpression::Dict(vec![]);
                }
                Some(DeclAction::InstanceFanOut(arms, span)) => {
                    node_mut.expr = expand_instance_to_dict(&arms, &span);
                }
                None => {}
            }
        }

        // String literals: no expansion needed.
        SurfaceExpression::StringLiteral { .. } => {}

        // Annotated VarRef: no expansion needed (annotation is type-level).
        SurfaceExpression::VarRef {
            annotation: Some(_),
            ..
        } => {}

        // Access: recurse into target.
        SurfaceExpression::Field {
            expr: Some(target), ..
        } => {
            expand_node(target);
        }
        SurfaceExpression::Field { expr: None, .. } => {}

        // Pipe: recurse into both sides.
        SurfaceExpression::Pipe { lhs, rhs, .. } => {
            expand_node(lhs);
            expand_node(rhs);
        }

        // Sequential: recurse into all expressions.
        SurfaceExpression::Sequential(exprs) => {
            for expr in exprs {
                expand_node(expr);
            }
        }

        // Dict: this is the primary expansion site.
        // Handles TypeAlias, InstanceDecl, ClassDecl expansion inside entries,
        // and spread-dict (...expr) rewriting to nested builtin-dict-merge calls.
        SurfaceExpression::Dict(_) => {
            // Take entries out of the Dict to avoid borrow conflicts.
            let mut entries = if let SurfaceExpression::Dict(entries) = &mut node_mut.expr {
                std::mem::take(entries)
            } else {
                unreachable!()
            };

            // Check for spread markers — spread-dict takes priority (matches
            // the old lowerer's early-return semantics).
            let has_spread = entries.iter().any(|e| {
                e.node.key.is_none()
                    && matches!(&e.node.value.expr, SurfaceExpression::Placeholder(..))
            });

            if has_spread {
                node_mut.expr = expand_spread_dict(&entries, &node_mut.span);
            } else {
                expand_dict_entries(&mut entries, &node_mut.span);
                node_mut.expr = SurfaceExpression::Dict(entries);
            }
        }

        // Call: recurse into func, args, and named args.
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            expand_node(func);
            for arg in args {
                expand_node(arg);
            }
            for named_arg_spanned in named_args {
                expand_node(&mut named_arg_spanned.node.value);
            }
        }

        // Fn: recurse into body.
        SurfaceExpression::Fn { body, .. } => {
            expand_node(body);
        }

        // TypeAssert: recurse into inner expression.
        SurfaceExpression::TypeAssert { expr: inner, .. } => {
            expand_node(inner);
        }

        // Quote: do NOT recurse (AST frozen).
        SurfaceExpression::Quote(_) => {}

        // Unquote/UnquoteSplice: recurse into inner expression.
        SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
            expand_node(inner);
        }

        // Match: recurse into scrutinee and arm bodies.
        SurfaceExpression::Match { scrutinee, arms } => {
            expand_node(scrutinee);
            for arm in arms {
                for body_expr in &mut arm.body {
                    expand_node(body_expr);
                }
            }
        }

        // Binding and pattern forms: recurse into bindings.
        SurfaceExpression::PatternDecl { bindings } => {
            for binding in bindings {
                expand_node(binding);
            }
        }
        SurfaceExpression::LetDecl { bindings } => {
            for binding in bindings {
                expand_node(binding);
            }
        }

        // Expand-stage output variants: recurse into children.
        SurfaceExpression::MakeVariant {
            ref mut payload, ..
        } => {
            expand_node(payload);
        }
        SurfaceExpression::TypeDecl { ref mut inner, .. } => {
            expand_node(inner);
        }
        SurfaceExpression::ReprDecl {
            ref mut inner,
            ref mut is_pred,
            ..
        } => {
            if let Some(is_pred_node) = is_pred {
                expand_node(is_pred_node);
            }
            expand_node(inner);
        }
    }
}

/// Expand declarations inside dict entries.
///
/// Handles three kinds of declaration expansion:
/// 1. **TypeAlias** → expanded constructor dict (TypeDecl/ReprDecl wrapping UnitVariant/MakeVariant)
/// 2. **InstanceDecl** → fanned-out method entries (one SurfaceEntry per method per arm)
/// 3. **ClassDecl** → empty dict placeholder (so the class name occupies a slot)
///
/// InstanceDecl fan-out can produce multiple output entries from a single input entry,
/// so when any declaration expansion is needed, the entire entries vec is rebuilt.
fn expand_dict_entries(entries: &mut Vec<Spanned<SurfaceEntry>>, _span: &Span) {
    // Check if any entry needs expansion that might change entry count.
    let needs_rebuild = entries.iter().any(|e| {
        matches!(
            &e.node.value.expr,
            SurfaceExpression::Decl(decl) if matches!(
                decl.as_ref(),
                SurfaceDeclaration::TypeAlias { .. }
                | SurfaceDeclaration::InstanceDecl { .. }
                | SurfaceDeclaration::ClassDecl { .. }
            )
        )
    });

    if !needs_rebuild {
        // No declaration entries — just recurse into all entries.
        for entry in entries.iter_mut() {
            if let Some(key_arc) = &mut entry.node.key {
                expand_node(key_arc);
            }
            expand_node(&mut entry.node.value);
        }
        return;
    }

    // Rebuild entries, expanding declarations.
    let old_entries = std::mem::take(entries);
    for entry in old_entries {
        expand_single_dict_entry(entry, entries);
    }
}

/// Expand a single dict entry, appending result(s) to `out`.
///
/// Declaration entries produce one or more expanded entries. Non-declaration
/// entries are recursed into and passed through unchanged.
fn expand_single_dict_entry(entry: Spanned<SurfaceEntry>, out: &mut Vec<Spanned<SurfaceEntry>>) {
    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
        match decl.as_ref() {
            SurfaceDeclaration::TypeAlias { params, body } => {
                let type_name_opt = extract_type_name_from_key(&entry.node.key);
                let params = params.clone();
                let body = Arc::clone(body);
                let value_span = entry.node.value.span.clone();
                let expanded_expr = expand_type_alias(type_name_opt, &params, &body, &value_span);
                out.push(Spanned::new(
                    SurfaceEntry {
                        key: entry.node.key.clone(),
                        value: Arc::new(SurfaceNode::new(expanded_expr, value_span)),
                        assigned_slot: std::sync::OnceLock::new(),
                    },
                    entry.span.clone(),
                ));
            }
            SurfaceDeclaration::InstanceDecl { arms, .. } => {
                let arms = arms.clone();
                expand_instance_in_dict(&entry, &arms, out);
            }
            SurfaceDeclaration::ClassDecl { .. } => {
                expand_class_in_dict(&entry, out);
            }
            _ => out.push(entry),
        }
    } else {
        // Non-declaration entry: recurse into key and value, then pass through.
        let mut entry = entry;
        if let Some(key_arc) = &mut entry.node.key {
            expand_node(key_arc);
        }
        expand_node(&mut entry.node.value);
        out.push(entry);
    }
}

// ---------------------------------------------------------------------------
// TypeAlias expansion
// ---------------------------------------------------------------------------

/// Expand a `TypeAlias` declaration into an equivalent `SurfaceExpression`.
///
/// Produces a tree of surface-level AST nodes:
/// - Outer: `TypeDecl { type_name, type_decl_id, params, inner }` (when type_name is available)
/// - Middle: `ReprDecl { repr, inner }` (when `repr:` metadata is present)
/// - Inner: `Dict` of constructor entries
///
/// Each constructor entry is either:
/// - **Unit**: `VarRef("Red") → UnitVariant { tycon: "Color", ctor: "Red", type_decl_id }`
/// - **Payload**: `VarRef("File") → Fn { params: [path, handle], body: MakeVariant { ... } }`
///
/// When a constructor has an annotation, it is wrapped in a call to
/// `builtin-make-annotated` (as a VarRef, resolved later by the resolver).
fn expand_type_alias(
    type_name_opt: Option<String>,
    params: &[(String, Option<Spanned<Annotation>>)],
    body: &Arc<SurfaceNode>,
    span: &Span,
) -> SurfaceExpression {
    // Extract `repr:` and `is:` metadata from the body.
    let (repr_opt, is_pred_opt) = extract_repr_and_is_from_body(&body.expr);

    // Extract constructors from the body.
    let ctors = extract_constructors_from_body(&body.expr);

    // Use the repr id directly when present (reserved TYPE_ID_* constants);
    // otherwise allocate a sequential id for user-defined types.
    let type_decl_id = match repr_opt {
        Some(id) => id,
        None => next_type_decl_id(),
    };

    let mut surface_entries: Vec<Spanned<SurfaceEntry>> = Vec::new();

    for ctor in ctors {
        let (tycon, ctor_name) = match &type_name_opt {
            Some(tn) => (tn.clone(), ctor.name.clone()),
            None => (String::new(), ctor.name.clone()),
        };
        let qualified_tag = if tycon.is_empty() {
            ctor_name.clone()
        } else {
            format!("{}.{}", tycon, ctor_name)
        };

        // Create the key for this constructor entry.
        let key = Some(Arc::new(SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: ctor.name.clone(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            span.clone(),
        )));

        // Create the value: either a unit variant or a constructor function.
        let value = if ctor.is_unit {
            // Unit constructor: SurfaceExpression::UnitVariant
            let variant_node = Arc::new(SurfaceNode::new(
                SurfaceExpression::UnitVariant {
                    tycon: tycon.clone(),
                    ctor: ctor_name.clone(),
                    type_decl_id,
                },
                span.clone(),
            ));

            if let Some(ann_entries) = &ctor.annotation {
                // Wrap in [builtin-make-annotated variant ann_dict]
                wrap_in_make_annotated(variant_node, ann_entries, None, span)
            } else {
                variant_node
            }
        } else {
            // Payload constructor: emit a function that accepts the fields as parameters
            // and constructs a MakeVariant with the payload dict.
            let fields = &ctor.fields;

            // Build one SurfaceParam per field.
            let fn_params: Vec<Spanned<SurfaceParam>> = fields
                .iter()
                .map(|field_name| {
                    Spanned::new(
                        SurfaceParam {
                            name: field_name.clone(),
                            annotation: None,
                            variadic: false,
                            resolved_annotation_type: crate::ast::TypeAnnotation::new(),
                        },
                        span.clone(),
                    )
                })
                .collect();

            // Build the payload dict: [field0: .field0  field1: .field1  ...]
            // Keys are VarRef (normal dict keys — create letrec bindings).
            // Values are leading-dot Field (skip the dict's letrec scope, resolve
            // from the parent scope = the fn's parameter scope).
            // This is exactly what a tinct programmer would write:
            //   [fn [let types] [types: .types]]
            let payload_entries: Vec<Spanned<SurfaceEntry>> = fields
                .iter()
                .map(|field_name| {
                    let entry_key = Some(Arc::new(SurfaceNode::new(
                        SurfaceExpression::VarRef {
                            name: field_name.clone(),
                            escaped: false,
                            resolution: crate::ast::Resolution::new(),
                            annotation: None,
                            do_infer_placeholder: false,
                        },
                        span.clone(),
                    )));
                    let entry_value = Arc::new(SurfaceNode::new(
                        SurfaceExpression::Field {
                            expr: None,
                            field: crate::ast::DotKey::Ident(field_name.clone()),
                            resolution: crate::ast::Resolution::new(),
                        },
                        span.clone(),
                    ));
                    Spanned::new(
                        SurfaceEntry {
                            key: entry_key,
                            value: entry_value,
                            assigned_slot: std::sync::OnceLock::new(),
                        },
                        span.clone(),
                    )
                })
                .collect();

            let payload_dict = Arc::new(SurfaceNode::new(
                SurfaceExpression::Dict(payload_entries),
                span.clone(),
            ));

            // Build the MakeVariant body.
            let variant_body = Arc::new(SurfaceNode::new(
                SurfaceExpression::MakeVariant {
                    tag: qualified_tag.clone(),
                    payload: payload_dict,
                    type_decl_id,
                },
                span.clone(),
            ));

            let fn_expr = Arc::new(SurfaceNode::new(
                SurfaceExpression::Fn {
                    return_ann: Some(Spanned::new(
                        Annotation::Simple(qualified_tag.clone()),
                        span.clone(),
                    )),
                    params: fn_params,
                    body: variant_body,
                    desugared: false,
                    resolved_captures: crate::ast::CapturesCell::new(),
                    resolved_return_annotation: crate::ast::TypeAnnotation::new(),
                },
                span.clone(),
            ));

            // Build annotation dict if present (explicit annotation or field annotations).
            let has_ann = ctor.annotation.is_some();
            let has_field_anns = !ctor.field_annotations.is_empty();
            if has_ann || has_field_anns {
                let field_ann_entries = if has_field_anns {
                    Some(&ctor.field_annotations)
                } else {
                    None
                };
                wrap_in_make_annotated(
                    fn_expr,
                    ctor.annotation.as_deref().unwrap_or(&[]),
                    field_ann_entries,
                    span,
                )
            } else {
                fn_expr
            }
        };

        surface_entries.push(Spanned::new(
            SurfaceEntry {
                key,
                value,
                assigned_slot: std::sync::OnceLock::new(),
            },
            span.clone(),
        ));
    }

    // When no constructors were extracted, the body is a structural type alias
    // (e.g., [type [let k v] [_@k: v]]). Preserve the original body — it IS the
    // type definition. The type-stage evaluator and type checker handle it directly.
    let inner = if surface_entries.is_empty() {
        body.expr.clone()
    } else {
        SurfaceExpression::Dict(surface_entries)
    };

    // If the body had `repr:` metadata, wrap in ReprDecl.
    let inner_expr = match repr_opt {
        Some(repr) => SurfaceExpression::ReprDecl {
            repr,
            is_pred: is_pred_opt,
            inner: Arc::new(SurfaceNode::new(inner, span.clone())),
        },
        None => inner,
    };

    // Wrap in TypeDecl when a type name is available.
    match type_name_opt {
        Some(type_name) => SurfaceExpression::TypeDecl {
            type_name,
            type_decl_id,
            params: params.to_vec(),
            inner: Arc::new(SurfaceNode::new(inner_expr, span.clone())),
        },
        None => inner_expr,
    }
}

// ---------------------------------------------------------------------------
// Annotation wrapping helper
// ---------------------------------------------------------------------------

/// Build a `[builtin-make-annotated expr ann_dict]` call node.
///
/// `ann_entries` are the explicit `@[...]` annotation entries.
/// `field_annotations` are per-field child role annotations (if any).
///
/// The annotation dict is a `SurfaceExpression::Dict` whose entries are the explicit
/// annotations plus an optional `field-annotations:` entry.
fn wrap_in_make_annotated(
    expr: Arc<SurfaceNode>,
    ann_entries: &[Spanned<SurfaceEntry>],
    field_annotations: Option<&indexmap::IndexMap<String, String>>,
    span: &Span,
) -> Arc<SurfaceNode> {
    let mut dict_entries: Vec<Spanned<SurfaceEntry>> = Vec::new();

    // Include explicit annotation entries.
    for se in ann_entries {
        dict_entries.push(se.clone());
    }

    // Append field-annotations entry if present.
    if let Some(fa) = field_annotations {
        if let Some(fa_entry) = build_field_annotations_surface_entry(fa, span) {
            dict_entries.push(fa_entry);
        }
    }

    let ann_dict = Arc::new(SurfaceNode::new(
        SurfaceExpression::Dict(dict_entries),
        span.clone(),
    ));

    // [builtin-make-annotated expr ann_dict]
    let func = Arc::new(SurfaceNode::new(
        SurfaceExpression::VarRef {
            name: "builtin-make-annotated".to_string(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            annotation: None,
            do_infer_placeholder: false,
        },
        span.clone(),
    ));

    Arc::new(SurfaceNode::new(
        SurfaceExpression::Call {
            func,
            args: vec![expr, ann_dict],
            named_args: vec![],
            implied: false,
            pipe_span: None,
        },
        span.clone(),
    ))
}

// ---------------------------------------------------------------------------
// Helper functions (adapted from lower.rs)
// ---------------------------------------------------------------------------

/// Extract the type name from a dict entry key for TypeAlias qualified tags.
///
/// Recognized key forms:
/// - `StringLiteral(s)` — plain string key
/// - `VarRef { name }` — bare identifier key (including annotated VarRef)
fn extract_type_name_from_key(key: &Option<Arc<SurfaceNode>>) -> Option<String> {
    match key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            _ => None,
        },
        None => None,
    }
}

/// Extract `repr:` and `is:` metadata from a TypeAlias body.
///
/// Handles two body forms:
/// 1. **Dict body** — `repr: 1` and/or `is: predicate` as named dict entries
/// 2. **VarRef@[PropertyDict] body** — `Int@[repr: 1 is: predicate]` as annotation entries
///
/// Returns `(Option<u64>, Option<Arc<SurfaceNode>>)` for repr and is_pred.
fn extract_repr_and_is_from_body(
    body: &SurfaceExpression,
) -> (Option<u64>, Option<Arc<SurfaceNode>>) {
    // VarRef@[PropertyDict] form.
    if let SurfaceExpression::VarRef {
        annotation: Some(ann),
        ..
    } = body
    {
        let prop_dict_entries = match &ann.node {
            Annotation::PropertyDict(entries) => Some(entries),
            Annotation::Annotated(_, inner) => {
                if let Annotation::PropertyDict(entries) = inner.as_ref() {
                    Some(entries)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(entries) = prop_dict_entries {
            return extract_repr_and_is_from_entries(entries);
        }
    }

    // Dict body form.
    let SurfaceExpression::Dict(entries) = body else {
        return (None, None);
    };
    extract_repr_and_is_from_entries(entries)
}

/// Scan a slice of `SurfaceEntry` for `repr:` and `is:` keys and extract their values.
fn extract_repr_and_is_from_entries(
    entries: &[Spanned<SurfaceEntry>],
) -> (Option<u64>, Option<Arc<SurfaceNode>>) {
    let mut repr_opt = None;
    let mut is_pred_opt = None;

    for entry in entries {
        let Some(key_node) = &entry.node.key else {
            continue;
        };
        let key_name = match &key_node.expr {
            SurfaceExpression::VarRef { name, .. } => name.as_str(),
            SurfaceExpression::StringLiteral { content, .. } => content.as_str(),
            _ => continue,
        };
        match key_name {
            "repr" => {
                if let SurfaceExpression::Int(n) = &entry.node.value.expr {
                    repr_opt = Some(*n as u64);
                }
            }
            "is" => {
                is_pred_opt = Some(Arc::clone(&entry.node.value));
            }
            _ => {}
        }
    }

    (repr_opt, is_pred_opt)
}

/// Returns true if a `SurfaceExpression` annotation contains `is:` or `repr:` keys,
/// indicating this is a structural alias declaration rather than a unit constructor.
fn has_structural_alias_metadata(ann: &Annotation) -> bool {
    let check_entries = |entries: &[Spanned<SurfaceEntry>]| {
        entries.iter().any(|e| {
            let Some(key_node) = &e.node.key else {
                return false;
            };
            matches!(
                &key_node.expr,
                SurfaceExpression::VarRef { name, .. } if name == "is" || name == "repr"
            )
        })
    };
    match ann {
        Annotation::PropertyDict(entries) => check_entries(entries),
        Annotation::Annotated(_, inner) => has_structural_alias_metadata(inner),
        _ => false,
    }
}

/// Extract the child role from a `@Child` or `@Child@[role: "..."]` annotation.
///
/// Returns `"One"` as the default role, or the explicit `role:` value.
fn child_role_from_annotation(ann: &Annotation) -> &'static str {
    if let Some(role_node) = ann.get_property(crate::ast::ANNOTATION_KEY_CHILD_ROLE) {
        if let SurfaceExpression::StringLiteral { content, .. } = &role_node.expr {
            match content.as_str() {
                "MapValues" => return "MapValues",
                "One" => return "One",
                _ => {}
            }
        }
    }
    "One"
}

/// Simplified constructor info for expansion.
struct ConstructorInfo {
    name: String,
    is_unit: bool,
    /// Annotation entries from `@[...]` on the constructor declaration.
    annotation: Option<Vec<Spanned<SurfaceEntry>>>,
    /// Field names for named-field constructors (empty for unit constructors).
    fields: Vec<String>,
    /// Per-field child role annotations from `@Child` on field keys.
    /// Maps field name -> role string ("One" or "MapValues").
    field_annotations: indexmap::IndexMap<String, String>,
}

/// Extract constructor information from a TypeAlias body.
///
/// Handles:
/// 1. Bare VarRef uppercase → unit constructor (e.g., `Red`, `None`)
/// 2. Annotated uppercase → unit constructor with annotation
/// 3. Call with uppercase func → unit constructor (e.g., `[Ok a]`)
/// 4. Dict with named uppercase-key entries → payload/unit constructors
fn extract_constructors_from_body(body: &SurfaceExpression) -> Vec<ConstructorInfo> {
    let mut ctors = Vec::new();

    fn is_ctor(s: &str) -> bool {
        crate::eval::is_constructor_name(s)
    }

    fn try_extract_one(expr: &SurfaceExpression, ctors: &mut Vec<ConstructorInfo>) {
        match expr {
            // Uppercase VarRef → unit constructor.
            // Exception: if annotation contains `is:` or `repr:`, this is a structural alias.
            SurfaceExpression::VarRef {
                name, annotation, ..
            } if is_ctor(name) => {
                if let Some(ann) = annotation {
                    if has_structural_alias_metadata(&ann.node) {
                        return;
                    }
                }
                let ann_entries = annotation.as_ref().and_then(|ann| match &ann.node {
                    Annotation::PropertyDict(entries) if !entries.is_empty() => {
                        Some(entries.clone())
                    }
                    _ => None,
                });
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    annotation: ann_entries,
                    fields: vec![],
                    field_annotations: indexmap::IndexMap::new(),
                });
            }
            // Call with uppercase func → unit constructor.
            SurfaceExpression::Call { func, .. } => {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if is_ctor(name) {
                        ctors.push(ConstructorInfo {
                            name: name.clone(),
                            is_unit: true,
                            annotation: None,
                            fields: vec![],
                            field_annotations: indexmap::IndexMap::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    match body {
        SurfaceExpression::Dict(entries) => {
            let has_named_uppercase_key = entries.iter().any(|e| {
                if let Some(k) = &e.node.key {
                    matches!(&k.expr, SurfaceExpression::VarRef { name, .. } if is_ctor(name))
                } else {
                    false
                }
            });

            if has_named_uppercase_key {
                for entry in entries {
                    if let Some(key_node) = &entry.node.key {
                        if let SurfaceExpression::VarRef {
                            name, annotation, ..
                        } = &key_node.expr
                        {
                            if !is_ctor(name) {
                                continue;
                            }
                            let ann_entries = annotation.as_ref().and_then(|ann| match &ann.node {
                                Annotation::PropertyDict(ann_entries)
                                    if !ann_entries.is_empty() =>
                                {
                                    Some(ann_entries.clone())
                                }
                                _ => None,
                            });
                            let mut fields: Vec<String> = Vec::new();
                            let mut field_anns = indexmap::IndexMap::new();
                            if let SurfaceExpression::Dict(field_entries) = &entry.node.value.expr {
                                for fe in field_entries {
                                    if let Some(k) = &fe.node.key {
                                        let (field_name, child_ann) = match &k.expr {
                                            SurfaceExpression::VarRef {
                                                name: fn_,
                                                annotation,
                                                ..
                                            } => {
                                                let child_ann =
                                                    annotation.as_ref().and_then(|ann| {
                                                        let is_child = match &ann.node {
                                                            Annotation::Simple(s) => s == "Child",
                                                            Annotation::Annotated(outer, _) => {
                                                                matches!(outer.as_ref(), Annotation::Simple(s) if s == "Child")
                                                            }
                                                            _ => false,
                                                        };
                                                        if is_child {
                                                            Some(&ann.node)
                                                        } else {
                                                            None
                                                        }
                                                    });
                                                (Some(fn_.clone()), child_ann)
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => (Some(content.clone()), None),
                                            _ => (None, None),
                                        };
                                        if let Some(fn_) = field_name {
                                            if let Some(ann) = child_ann {
                                                let role = child_role_from_annotation(ann);
                                                field_anns.insert(fn_.clone(), role.to_string());
                                            }
                                            fields.push(fn_);
                                        }
                                    }
                                }
                            }
                            let is_unit = fields.is_empty();
                            ctors.push(ConstructorInfo {
                                name: name.clone(),
                                is_unit,
                                annotation: ann_entries,
                                fields,
                                field_annotations: field_anns,
                            });
                        }
                    } else {
                        // Positional entry: bare uppercase VarRef → unit constructor.
                        try_extract_one(&entry.node.value.expr, &mut ctors);
                    }
                }
            } else {
                // Old body form: union of positional unit constructors.
                for entry in entries {
                    if entry.node.key.is_none() {
                        try_extract_one(&entry.node.value.expr, &mut ctors);
                    }
                }
            }
        }
        other => try_extract_one(other, &mut ctors),
    }
    ctors
}

/// Build a `SurfaceExpression::Dict` entry for `field-annotations:` metadata.
///
/// Produces `"field-annotations": { field_name: { role: "One"|"MapValues" }, ... }`.
/// Returns `None` if `field_annotations` is empty.
fn build_field_annotations_surface_entry(
    field_annotations: &indexmap::IndexMap<String, String>,
    span: &Span,
) -> Option<Spanned<SurfaceEntry>> {
    if field_annotations.is_empty() {
        return None;
    }

    // Build inner dicts: for each field, {role: "RoleName"}
    let field_entries: Vec<Spanned<SurfaceEntry>> = field_annotations
        .iter()
        .map(|(field_name, role)| {
            // Inner dict: {role: "One"} or {role: "MapValues"}
            let role_key = Some(Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: crate::ast::ANNOTATION_KEY_CHILD_ROLE.to_string(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    annotation: None,
                    do_infer_placeholder: false,
                },
                span.clone(),
            )));
            let role_value = Arc::new(SurfaceNode::new(
                SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: role.clone(),
                },
                span.clone(),
            ));
            let role_entry = Spanned::new(
                SurfaceEntry {
                    key: role_key,
                    value: role_value,
                    assigned_slot: std::sync::OnceLock::new(),
                },
                span.clone(),
            );
            let inner_dict = Arc::new(SurfaceNode::new(
                SurfaceExpression::Dict(vec![role_entry]),
                span.clone(),
            ));
            // Outer entry: field_name: {role: "One"|"MapValues"}
            let key = Some(Arc::new(SurfaceNode::new(
                SurfaceExpression::VarRef {
                    name: field_name.clone(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    annotation: None,
                    do_infer_placeholder: false,
                },
                span.clone(),
            )));
            Spanned::new(
                SurfaceEntry {
                    key,
                    value: inner_dict,
                    assigned_slot: std::sync::OnceLock::new(),
                },
                span.clone(),
            )
        })
        .collect();

    let field_ann_dict = Arc::new(SurfaceNode::new(
        SurfaceExpression::Dict(field_entries),
        span.clone(),
    ));

    // Wrap as "field-annotations": {field_name: {role: "..."}, ...}
    let key = Some(Arc::new(SurfaceNode::new(
        SurfaceExpression::VarRef {
            name: "field-annotations".to_string(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            annotation: None,
            do_infer_placeholder: false,
        },
        span.clone(),
    )));
    Some(Spanned::new(
        SurfaceEntry {
            key,
            value: field_ann_dict,
            assigned_slot: std::sync::OnceLock::new(),
        },
        span.clone(),
    ))
}

// ---------------------------------------------------------------------------
// InstanceDecl fan-out (T-2217)
// ---------------------------------------------------------------------------

/// Fan out an `InstanceDecl` inside a dict entry into individual method entries.
///
/// For named instances (entry has a key), emits:
/// 1. The outer key binding entry pointing to a dict of all methods
/// 2. One entry per method per arm (with type guard annotations injected)
///
/// For anonymous instances (no key), emits only the method entries.
///
/// The instance arm pattern's type annotations are injected into matching Fn
/// parameters so the lowerer can propagate them to `CoreParam.resolved_type`
/// for EffectPerform dispatch.
fn expand_instance_in_dict(
    entry: &Spanned<SurfaceEntry>,
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)],
    out: &mut Vec<Spanned<SurfaceEntry>>,
) {
    // Named instance: emit outer key binding pointing to the methods dict.
    if let Some(key) = &entry.node.key {
        let method_dict_expr = expand_instance_to_dict(arms, &entry.span);
        let value = Arc::new(SurfaceNode::new(
            method_dict_expr,
            entry.node.value.span.clone(),
        ));
        out.push(Spanned::new(
            SurfaceEntry {
                key: Some(key.clone()),
                value,
                assigned_slot: std::sync::OnceLock::new(),
            },
            entry.span.clone(),
        ));
    }

    // Fan out individual method entries.
    for (pattern, method_entries) in arms {
        let arm_annotations = extract_arm_annotations(&pattern.expr);
        for me in method_entries {
            let mut method_value = me.node.value.clone();
            // Inject type guard annotations from the arm pattern into Fn params.
            inject_arm_annotations_into_fn(&mut method_value, &arm_annotations);
            // Recurse into the method value.
            expand_node(&mut method_value);

            out.push(Spanned::new(
                SurfaceEntry {
                    key: me.node.key.clone(),
                    value: method_value,
                    assigned_slot: std::sync::OnceLock::new(),
                },
                me.span.clone(),
            ));
        }
    }
}

/// Expand a standalone `InstanceDecl` (expression position) into a dict of method entries.
///
/// Same fan-out as `expand_instance_in_dict` but produces a `SurfaceExpression::Dict`
/// containing all method entries (no outer key binding).
fn expand_instance_to_dict(
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)],
    _span: &Span,
) -> SurfaceExpression {
    let mut entries: Vec<Spanned<SurfaceEntry>> = Vec::new();
    for (pattern, method_entries) in arms {
        let arm_annotations = extract_arm_annotations(&pattern.expr);
        for me in method_entries {
            let mut method_value = me.node.value.clone();
            inject_arm_annotations_into_fn(&mut method_value, &arm_annotations);
            expand_node(&mut method_value);
            entries.push(Spanned::new(
                SurfaceEntry {
                    key: me.node.key.clone(),
                    value: method_value,
                    assigned_slot: std::sync::OnceLock::new(),
                },
                me.span.clone(),
            ));
        }
    }
    SurfaceExpression::Dict(entries)
}

/// Extract (binding_name, annotation) pairs from an instance arm pattern.
///
/// For `[let h@File]`, returns `[("h", @File)]`.
/// For `[let a@String b@String c]`, returns `[("a", @String), ("b", @String)]`.
/// Bindings without type annotations are omitted.
/// `bind: [...]` TypeVar declarations are skipped (same filter as `extract_instance_type_guards`).
fn extract_arm_annotations(arm_pattern: &SurfaceExpression) -> Vec<(String, Spanned<Annotation>)> {
    let bindings = match arm_pattern {
        SurfaceExpression::LetDecl { bindings } => bindings,
        _ => return vec![],
    };
    bindings
        .iter()
        .filter(|binding| {
            // Skip `bind: [...]` TypeVar declarations.
            if let SurfaceExpression::VarRef {
                name, annotation, ..
            } = &binding.expr
            {
                if name.as_str() == "bind" {
                    if let Some(ann) = annotation {
                        if matches!(ann.node, Annotation::PropertyDict(_)) {
                            return false;
                        }
                    }
                }
            }
            true
        })
        .filter_map(|binding_spanned| {
            if let SurfaceExpression::VarRef {
                name,
                annotation: Some(ann),
                ..
            } = &binding_spanned.expr
            {
                Some((name.clone(), ann.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// Inject type annotations from an instance arm pattern into a method body's Fn params.
///
/// For each (binding_name, annotation) pair from the arm pattern, finds the matching
/// param in the Fn by name and sets its annotation if it has none. This propagates
/// the instance type constraint through the normal type checker path, ultimately
/// setting `CoreParam.resolved_type` for EffectPerform dispatch.
fn inject_arm_annotations_into_fn(
    method_body: &mut Arc<SurfaceNode>,
    arm_annotations: &[(String, Spanned<Annotation>)],
) {
    if arm_annotations.is_empty() {
        return;
    }
    let node_mut = Arc::make_mut(method_body);
    if let SurfaceExpression::Fn { params, .. } = &mut node_mut.expr {
        for param in params.iter_mut() {
            if param.node.annotation.is_none() {
                if let Some((_, ann)) = arm_annotations
                    .iter()
                    .find(|(name, _)| name == &param.node.name)
                {
                    param.node.annotation = Some(ann.clone());
                }
            }
        }
    }
}

/// Extract the dispatch tag from an annotation.
///
/// Returns the type constructor name for dispatch matching:
/// - `@File` → `Some("File")`
/// - `@Integer` → `Some("Integer")`
// ---------------------------------------------------------------------------
// ClassDecl placeholder (T-2218)
// ---------------------------------------------------------------------------

/// Replace a `ClassDecl` dict entry with an empty-dict placeholder.
///
/// Named ClassDecl entries get a single entry with an empty dict value so the
/// class name occupies a slot for leading-dot re-exports. Anonymous ClassDecl
/// entries are dropped (nothing to emit).
fn expand_class_in_dict(entry: &Spanned<SurfaceEntry>, out: &mut Vec<Spanned<SurfaceEntry>>) {
    if entry.node.key.is_some() {
        out.push(Spanned::new(
            SurfaceEntry {
                key: entry.node.key.clone(),
                value: Arc::new(SurfaceNode::new(
                    SurfaceExpression::Dict(vec![]),
                    entry.node.value.span.clone(),
                )),
                assigned_slot: std::sync::OnceLock::new(),
            },
            entry.span.clone(),
        ));
    }
    // Anonymous ClassDecl: nothing to emit.
}

// ---------------------------------------------------------------------------
// Spread-dict expansion (T-2218)
// ---------------------------------------------------------------------------

/// Expand a spread-dict `[a: 1  ...rest  b: 2]` into nested `builtin-dict-merge` calls.
///
/// Segments entries around Placeholder spread markers, then builds
/// left-associative nested calls:
/// ```text
/// Call(VarRef("builtin-dict-merge"),
///   [Call(VarRef("builtin-dict-merge"), [Dict([a: 1]), rest]),
///    Dict([b: 2])])
/// ```
///
/// The `builtin-dict-merge` reference is a `VarRef` — the resolver finds
/// the address naturally. No manual de Bruijn lookup needed.
fn expand_spread_dict(entries: &[Spanned<SurfaceEntry>], span: &Span) -> SurfaceExpression {
    // Segment entry indices around spread markers.
    // segments[i] = indices of regular entries before rest_nodes[i].
    let mut segments: Vec<Vec<usize>> = vec![vec![]];
    let mut rest_indices: Vec<usize> = vec![];
    for (idx, se) in entries.iter().enumerate() {
        if se.node.key.is_none() {
            if let SurfaceExpression::Placeholder(..) = &se.node.value.expr {
                rest_indices.push(idx);
                segments.push(vec![]);
                continue;
            }
        }
        segments.last_mut().unwrap().push(idx);
    }

    let make_merge_ref = || -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: "builtin-dict-merge".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            span.clone(),
        ))
    };

    let build_segment_dict = |idxs: &[usize]| -> Arc<SurfaceNode> {
        let seg_entries: Vec<Spanned<SurfaceEntry>> =
            idxs.iter().map(|&i| entries[i].clone()).collect();
        Arc::new(SurfaceNode::new(
            SurfaceExpression::Dict(seg_entries),
            span.clone(),
        ))
    };

    // Build nested builtin-dict-merge calls left-associatively.
    // acc starts as the first segment dict, then folds over (rest, next_seg) pairs.
    let mut acc: Arc<SurfaceNode> = build_segment_dict(&segments[0]);

    for (i, &ri) in rest_indices.iter().enumerate() {
        // The spread value — keep the Placeholder node as-is. The resolver/lowerer
        // will handle it downstream.
        let rest_value = Arc::clone(&entries[ri].node.value);

        // builtin-dict-merge(acc, rest)
        acc = Arc::new(SurfaceNode::new(
            SurfaceExpression::Call {
                func: make_merge_ref(),
                args: vec![acc, rest_value],
                named_args: vec![],
                implied: false,
                pipe_span: None,
            },
            span.clone(),
        ));

        // builtin-dict-merge(acc, next_segment) if non-empty
        if i + 1 < segments.len() && !segments[i + 1].is_empty() {
            let seg = build_segment_dict(&segments[i + 1]);
            acc = Arc::new(SurfaceNode::new(
                SurfaceExpression::Call {
                    func: make_merge_ref(),
                    args: vec![acc, seg],
                    named_args: vec![],
                    implied: false,
                    pipe_span: None,
                },
                span.clone(),
            ));
        }
    }

    // Unwrap the outermost node to get the expression — the caller sets
    // node_mut.expr directly.
    Arc::try_unwrap(acc)
        .unwrap_or_else(|arc| (*arc).clone())
        .expr
}

#[cfg(test)]
/// **Test categorization: Category 4 — Rust unit tests for AST transformations**
///
/// These tests verify that the expand stage produces the correct SurfaceExpression variants
/// (UnitVariant, MakeVariant, TypeDecl, ReprDecl). They check AST discriminants and structure
/// rather than end-to-end tinct behavior. There is no tinct-surface syntax that directly
/// produces these variants — they are internal representations created by the expand stage
/// from `[type ...]` declarations before the resolver runs.
///
/// Category 4 tests check implementation details: AST node types, field values, and
/// transformation correctness. They complement corpus tests (category 1) which verify
/// that the full pipeline (parse → desugar → expand → resolve → lower → eval) produces
/// correct runtime results for user-visible tinct code.
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Parse source, desugar, and expand. Returns the expanded SurfaceProgram.
    fn parse_and_expand(source: &str) -> SurfaceProgram {
        let parsed = crate::parser::parse(source, Arc::from("test")).unwrap();
        let desugared = crate::desugar::desugar_program_full(&parsed.program);
        expand_program(&desugared)
    }

    /// Extract the first expression item from the first document of a program.
    fn first_expr(program: &SurfaceProgram) -> &SurfaceExpression {
        let doc = &program.documents[0].node;
        match &doc.items[0] {
            SurfaceItem::Expr(node) => &node.expr,
            SurfaceItem::Decl(_) => panic!("expected Expr item, got Decl"),
        }
    }

    /// Given a Dict expression, find the entry with the given key name and return
    /// a reference to its value expression. Matches both StringLiteral keys
    /// (parser-produced dict keys) and VarRef keys (expand-produced constructor keys).
    fn dict_entry_value<'a>(expr: &'a SurfaceExpression, key_name: &str) -> &'a SurfaceExpression {
        let entries = match expr {
            SurfaceExpression::Dict(entries) => entries,
            other => panic!("expected Dict, got {:?}", std::mem::discriminant(other)),
        };
        for entry in entries {
            if let Some(key_node) = &entry.node.key {
                let matches = match &key_node.expr {
                    SurfaceExpression::StringLiteral { content, .. } => content == key_name,
                    SurfaceExpression::VarRef { name, .. } => name == key_name,
                    _ => false,
                };
                if matches {
                    return &entry.node.value.expr;
                }
            }
        }
        panic!("no entry with key {:?} found in dict", key_name);
    }

    /// Return the entries vec from a Dict expression.
    fn dict_entries(expr: &SurfaceExpression) -> &Vec<Spanned<SurfaceEntry>> {
        match expr {
            SurfaceExpression::Dict(entries) => entries,
            other => panic!("expected Dict, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn test_expand_unit_type_alias() {
        let program = parse_and_expand("[Color: [type Red Green Blue]]");
        let top = first_expr(&program);

        // Top-level should be a Dict with one entry "Color".
        let color_expr = dict_entry_value(top, "Color");

        // "Color" should be a TypeDecl.
        let inner = match color_expr {
            SurfaceExpression::TypeDecl {
                type_name, inner, ..
            } => {
                assert_eq!(type_name, "Color");
                &inner.expr
            }
            other => panic!("expected TypeDecl, got {:?}", std::mem::discriminant(other)),
        };

        // The inner should be a Dict with 3 entries (Red, Green, Blue).
        let ctor_entries = dict_entries(inner);
        assert_eq!(ctor_entries.len(), 3);

        // Each entry value should be a UnitVariant.
        let expected_ctors = ["Red", "Green", "Blue"];
        for (entry, expected_name) in ctor_entries.iter().zip(expected_ctors.iter()) {
            let key_node = entry
                .node
                .key
                .as_ref()
                .expect("constructor entry should have a key");
            match &key_node.expr {
                SurfaceExpression::VarRef { name, .. } => {
                    assert_eq!(name, expected_name);
                }
                other => panic!(
                    "expected VarRef key, got {:?}",
                    std::mem::discriminant(other)
                ),
            }
            match &entry.node.value.expr {
                SurfaceExpression::UnitVariant { tycon, ctor, .. } => {
                    assert_eq!(tycon, "Color");
                    assert_eq!(ctor, *expected_name);
                }
                other => panic!(
                    "expected UnitVariant for {}, got {:?}",
                    expected_name,
                    std::mem::discriminant(other)
                ),
            }
        }
    }

    #[test]
    fn test_expand_payload_type_alias() {
        let program = parse_and_expand("[Shape: [type Circle: [r: Int]]]");
        let top = first_expr(&program);

        // Top-level should be a Dict with one entry "Shape".
        let shape_expr = dict_entry_value(top, "Shape");

        // "Shape" should be a TypeDecl.
        let inner = match shape_expr {
            SurfaceExpression::TypeDecl {
                type_name, inner, ..
            } => {
                assert_eq!(type_name, "Shape");
                &inner.expr
            }
            other => panic!("expected TypeDecl, got {:?}", std::mem::discriminant(other)),
        };

        // Inner should be a Dict with one entry "Circle".
        let ctor_entries = dict_entries(inner);
        assert_eq!(ctor_entries.len(), 1);

        let circle_key = ctor_entries[0].node.key.as_ref().expect("should have key");
        match &circle_key.expr {
            SurfaceExpression::VarRef { name, .. } => assert_eq!(name, "Circle"),
            other => panic!(
                "expected VarRef key, got {:?}",
                std::mem::discriminant(other)
            ),
        }

        // Circle's value should be an Fn (payload constructor function).
        match &ctor_entries[0].node.value.expr {
            SurfaceExpression::Fn { params, body, .. } => {
                // Should have one param "r".
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "r");

                // The body should be a MakeVariant.
                match &body.expr {
                    SurfaceExpression::MakeVariant { tag, .. } => {
                        assert_eq!(tag, "Shape.Circle");
                    }
                    other => panic!(
                        "expected MakeVariant body, got {:?}",
                        std::mem::discriminant(other)
                    ),
                }
            }
            other => panic!(
                "expected Fn for Circle, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn test_expand_class_placeholder() {
        let program = parse_and_expand("[Printable: [class [let a]]]");
        let top = first_expr(&program);

        // Top-level should be a Dict with one entry "Printable".
        let printable_expr = dict_entry_value(top, "Printable");

        // "Printable" should be replaced with an empty Dict.
        let entries = dict_entries(printable_expr);
        assert!(
            entries.is_empty(),
            "ClassDecl should expand to empty dict, got {} entries",
            entries.len()
        );
    }

    #[test]
    fn test_expand_identity() {
        let program = parse_and_expand("[x: 1 y: 2]");
        let top = first_expr(&program);

        // Should be a Dict with two entries.
        let entries = dict_entries(top);
        assert_eq!(entries.len(), 2);

        // First entry: x: 1
        // Parser produces StringLiteral keys for dict entries (not VarRef).
        let x_key = entries[0].node.key.as_ref().expect("should have key");
        match &x_key.expr {
            SurfaceExpression::StringLiteral { content, .. } => assert_eq!(content, "x"),
            other => panic!(
                "expected StringLiteral key, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        match &entries[0].node.value.expr {
            SurfaceExpression::Int(1) => {}
            other => panic!("expected Int(1), got {:?}", std::mem::discriminant(other)),
        }

        // Second entry: y: 2
        let y_key = entries[1].node.key.as_ref().expect("should have key");
        match &y_key.expr {
            SurfaceExpression::StringLiteral { content, .. } => assert_eq!(content, "y"),
            other => panic!(
                "expected StringLiteral key, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        match &entries[1].node.value.expr {
            SurfaceExpression::Int(2) => {}
            other => panic!("expected Int(2), got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn test_expand_standalone_type_alias() {
        // A standalone [type Red Green] not inside a dict entry becomes Dict(vec![]).
        // At the top level, the parser produces SurfaceItem::Decl, which expand_document
        // passes through. To test the expand_node Decl arm (TypeAlias → empty dict),
        // we put the type decl inside a Call as an argument: [identity [type Red Green]].
        // The expand_node Call arm recurses into args, hitting the Decl arm on the arg.
        let program = parse_and_expand("[identity [type Red Green]]");
        let top = first_expr(&program);

        // Top-level should be a Call (to "identity").
        let args = match top {
            SurfaceExpression::Call { func, args, .. } => {
                match &func.expr {
                    SurfaceExpression::VarRef { name, .. } => {
                        assert_eq!(name, "identity");
                    }
                    other => panic!(
                        "expected VarRef func, got {:?}",
                        std::mem::discriminant(other)
                    ),
                }
                args
            }
            other => panic!("expected Call, got {:?}", std::mem::discriminant(other)),
        };

        // The first (and only) arg should be Dict(vec![]) — standalone TypeAlias
        // in expression position is expanded to an empty dict.
        assert_eq!(args.len(), 1);
        let entries = dict_entries(&args[0].expr);
        assert!(
            entries.is_empty(),
            "standalone type alias should expand to empty dict, got {} entries",
            entries.len()
        );
    }
}
