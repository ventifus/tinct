//! Generic TypeNode traversal walkers (T-1062).
//! Scaffolding for the equirecursive types sprint chain (S-860 CheckerType migration).
#![allow(dead_code)]
//!
//!
//! These functions operate on `Value::Variant` TypeNode values produced by the TypeNode ADT
//! declared in `stdlib/prelude.llt` (T-1058). They coexist with the existing Rust `Type`
//! enum walkers (`has_inference_vars`, `collect_type_vars`, etc.) in `src/type_def.rs`
//! until the CheckerType migration (S-860) is complete.
//!
//! ## Design: Direct Structure Walking
//!
//! Rather than calling the tinct `TypeNode.children` function at each node (which would
//! require a full type-stage EvalContext round-trip per call), these walkers directly
//! pattern-match on the Variant tag and access `@Child` fields from the payload dict.
//! This matches the TypeNode ADT declaration (T-1058) exactly:
//!
//! | Constructor        | @Child fields           | Role       |
//! |--------------------|-------------------------|------------|
//! | `TypeNode.Union`   | `types`                 | Seq        |
//! | `TypeNode.Intersect` | `types`               | Seq        |
//! | `TypeNode.Record`  | `fields`                | MapValues  |
//! | `TypeNode.TypeApplication` | `ctor`, `args` | One, Seq   |
//! | `TypeNode.Arrow`   | `params`, `result`      | Seq, One   |
//! | `TypeNode.Recursive` | `body`                | One        |
//! | All others         | (none — leaf nodes)     | —          |
//!
//! Leaf constructors: `Int`, `Float`, `String`, `Bool`, `Absent`, `Unknown`, `Never`,
//! `TypeConstructor`, `RecursiveRef`, `TypeVar`.
//!
//! ## EvalContext Requirement
//!
//! TypeNode values store child nodes as `ThunkId`s inside their payload dict. Accessing
//! them requires an `Arc<EvalContext>` to call `materialize_sync`. Callers must provide
//! one; the helpers in this module use `materialize_sync` (blocking) to keep the walking
//! interface synchronous, consistent with type checking's non-async execution model.
//!
//! ## Scope
//!
//! These walkers implement traversal for the equirecursive type sprint (T-1062). They
//! replicate, at the TypeNode Value level, the same queries that `has_inference_vars` and
//! `collect_type_vars` answer at the Rust `Type` enum level. Full migration of the type
//! checker to use these walkers exclusively is S-860 (CheckerType migration).

use std::collections::HashSet;
use std::sync::Arc;

use crate::value::{Key, Value};

// ── Seq iteration ──────────────────────────────────────────────────────────────

/// Collect all elements from a `Seq.Cons` / `Seq.Nil` linked-list Value into a `Vec<Value>`.
///
/// Returns an empty `Vec` for:
/// - `Seq.Nil` (empty sequence)
/// - Any non-Sequence value (graceful degradation — a missing @Child Seq field returns
///   `[]` / empty dict from the prelude, which is not a Seq but also has no children)
///
/// Materializes each `Seq.Cons` payload synchronously. Iteration is bounded by the length
/// of the sequence; infinite sequences will hang the type checker (never constructed from
/// finite type annotations in practice).
///
/// ## Why this is distinct from `typecheck_annot::collect_typenode_seq`
///
/// `collect_typenode_seq` (in `typecheck_annot.rs`) collects Seq elements and immediately
/// converts each one to a `Type` via `typenode_value_to_type`, returning `Option<Vec<Type>>`.
/// This function differs in two ways:
///
/// 1. **Return type**: `Vec<Value>` vs `Option<Vec<Type>>` — this function is a generic
///    walker that leaves elements as raw Values, suitable for any Seq traversal.
/// 2. **Failure semantics**: silently stops iteration on any materialization error (soft
///    degradation — partial results are acceptable for traversal). `collect_typenode_seq`
///    returns `None` on any malformed element (hard error — the caller needs all elements).
///
/// These distinct contracts mean consolidation would force one to adopt the other's semantics.
/// Both implementations are intentional and must remain separate.
fn collect_seq_sync(seq: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Vec<Value> {
    let mut result = Vec::new();
    let mut current = seq.clone();

    loop {
        // Unwrap annotated wrappers transparently (TypeNode constructors may be annotated).
        let current_inner = match &current {
            Value::Annotated { inner, .. } => *inner.clone(),
            other => other.clone(),
        };

        match &current_inner {
            // Seq.Nil — empty sequence, done.
            Value::Variant { tag, payload: None } if tag == "Seq.Nil" => break,

            // Seq.Cons { head: ThunkId, tail: ThunkId } — extract head, advance to tail.
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                let payload_thunk = ctx.get_thunk(*payload_id);
                let payload_val = match crate::eval::materialize_sync(&payload_thunk, None, ctx) {
                    Ok(v) => v,
                    Err(_) => break, // Materialization error — stop iteration gracefully.
                };

                let (head_id, tail_id) = match &payload_val {
                    Value::Dict(d) => {
                        let head = d.get(&Key::String("head".into())).copied();
                        let tail = d.get(&Key::String("tail".into())).copied();
                        match (head, tail) {
                            (Some(h), Some(t)) => (h, t),
                            _ => break, // Malformed Seq.Cons — stop gracefully.
                        }
                    }
                    _ => break, // Unexpected payload shape — stop gracefully.
                };

                // Materialize the head element (the TypeNode child).
                let head_thunk = ctx.get_thunk(head_id);
                match crate::eval::materialize_sync(&head_thunk, None, ctx) {
                    Ok(head_val) => result.push(head_val),
                    Err(_) => break, // Materialization error — stop gracefully.
                }

                // Advance to tail.
                let tail_thunk = ctx.get_thunk(tail_id);
                match crate::eval::materialize_sync(&tail_thunk, None, ctx) {
                    Ok(tail_val) => current = tail_val,
                    Err(_) => break,
                }
            }

            // Not a Seq — empty child set (leaf field or missing @Child).
            _ => break,
        }
    }

    result
}

// ── Payload field access ───────────────────────────────────────────────────────

/// Materialize a single named field from a TypeNode Variant's payload dict.
///
/// Returns `None` if:
/// - The Variant has no payload.
/// - The payload does not materialize to a `Value::Dict`.
/// - The named field is absent from the payload dict.
/// - Materialization of the payload or the field thunk fails.
///
/// Unwraps `Value::Annotated` on the node before reading the payload (TypeNode
/// constructors annotated with `@[as-type: ...]` are `Value::Annotated` wrappers
/// whose `inner` is the bare `Value::Variant`).
fn get_payload_field(
    node: &Value,
    field: &str,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Value> {
    // Unwrap annotated wrapper transparently.
    let inner = match node {
        Value::Annotated { inner, .. } => inner.as_ref(),
        other => other,
    };

    let payload_id = match inner {
        Value::Variant {
            payload: Some(id), ..
        } => *id,
        _ => return None,
    };

    let payload_thunk = ctx.get_thunk(payload_id);
    let payload_val = crate::eval::materialize_sync(&payload_thunk, None, ctx).ok()?;

    let field_id = match &payload_val {
        Value::Dict(d) => *d.get(&Key::String(field.into()))?,
        _ => return None,
    };

    let field_thunk = ctx.get_thunk(field_id);
    crate::eval::materialize_sync(&field_thunk, None, ctx).ok()
}

/// Collect all values from a TypeNode Variant's `Map String TypeNode` payload field.
///
/// Used for `TypeNode.Record`'s `fields` field, which has role `MapValues`:
/// the field is a string-keyed dict whose values are TypeNode children.
///
/// Returns an empty `Vec` if the field is absent, not a `Value::Dict`, or any
/// materialization step fails. Key order is `IndexMap` insertion order (preserved from
/// the source dict literal). Keys are discarded — only values (TypeNode children) matter
/// for traversal.
fn get_map_values_field(
    node: &Value,
    field: &str,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Vec<Value> {
    let map_val = match get_payload_field(node, field, ctx) {
        Some(v) => v,
        None => return Vec::new(),
    };

    let dict = match &map_val {
        Value::Dict(d) => d.clone(),
        _ => return Vec::new(),
    };

    let mut values = Vec::with_capacity(dict.len());
    for (_key, thunk_id) in &dict {
        let thunk = ctx.get_thunk(*thunk_id);
        if let Ok(v) = crate::eval::materialize_sync(&thunk, None, ctx) {
            values.push(v);
        }
    }
    values
}

// ── TypeNode tag helpers ───────────────────────────────────────────────────────

/// Extract the TypeNode tag string from a Value, unwrapping `Value::Annotated` first.
///
/// Returns `None` for non-Variant values (e.g., old-style kind-keyed dicts).
fn typenode_tag(node: &Value) -> Option<&str> {
    match node {
        Value::Annotated { inner, .. } => typenode_tag(inner),
        Value::Variant { tag, .. } => Some(tag.as_str()),
        _ => None,
    }
}

/// Returns `true` iff the node is a `TypeNode.TypeVar` Variant.
///
/// TypeVar nodes represent inference variables — the TypeNode-level equivalent of
/// `Type::TypeVar(name, level)` in the Rust enum representation.
fn is_typenode_typevar(node: &Value) -> bool {
    typenode_tag(node) == Some("TypeNode.TypeVar")
}

// ── Generic TypeNode walk ──────────────────────────────────────────────────────

/// Generic pre-order walk over a TypeNode Value.
///
/// Visits every node in the TypeNode tree in pre-order: `f` is called on the current node
/// first, then on each child (determined by the `@Child` field annotations in the TypeNode
/// ADT declaration). If `f` returns `Some(result)`, the walk short-circuits and returns
/// that result immediately without visiting further nodes.
///
/// Returns `None` if `f` never returns `Some` on any visited node.
///
/// ## TypeNode children by constructor
///
/// | Tag                        | Children                              |
/// |----------------------------|---------------------------------------|
/// | `TypeNode.Union`           | `types` (Seq of TypeNode)             |
/// | `TypeNode.Intersect`       | `types` (Seq of TypeNode)             |
/// | `TypeNode.Record`          | `fields` values (Map String TypeNode) |
/// | `TypeNode.TypeApplication` | `ctor` (One), `args` (Seq of TypeNode)|
/// | `TypeNode.Arrow`           | `params` (Seq of TypeNode), `result` (One) |
/// | `TypeNode.Recursive`       | `body` (One TypeNode)                 |
/// | All others                 | No children (leaf nodes)              |
///
/// Leaf constructors: `TypeNode.Int`, `TypeNode.Float`, `TypeNode.String`,
/// `TypeNode.Bool`, `TypeNode.Absent`, `TypeNode.Unknown`, `TypeNode.Never`,
/// `TypeNode.TypeConstructor`, `TypeNode.RecursiveRef`, `TypeNode.TypeVar`.
///
/// ## Context requirement
///
/// `ctx` is used to materialize ThunkIds inside Variant payloads. It must be a valid
/// type-stage EvalContext (produced by `build_type_stage_env`). Callers typically obtain
/// one via `crate::imports::build_type_stage_env()` followed by
/// `EvalContext::new_empty(dir, env, false)`.
///
/// ## Early exit and cycles
///
/// Short-circuit (`Some(R)`) stops traversal immediately. This function does NOT protect
/// against cycles — TypeNode values produced by `mu` or named-alias expansion contain
/// `TypeNode.RecursiveRef` leaves (not back-edges), so the tree is always finite and
/// acyclic for well-formed TypeNode values. Walking into a `TypeNode.Recursive.body`
/// will encounter `TypeNode.RecursiveRef` nodes at recursive positions (which are leaves),
/// not the full `TypeNode.Recursive` node again.
pub(crate) fn walk_typenode<F, R>(
    node: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
    f: &mut F,
) -> Option<R>
where
    F: FnMut(&Value) -> Option<R>,
{
    // Pre-order: visit this node first.
    if let Some(result) = f(node) {
        return Some(result);
    }

    // Determine the tag (unwrapping Annotated wrappers).
    let tag = typenode_tag(node)?; // Non-Variant — no children to walk.

    match tag {
        // ── Union / Intersect: `types@Child: [Seq TypeNode]` ──────────────────
        "TypeNode.Union" | "TypeNode.Intersect" => {
            if let Some(types_val) = get_payload_field(node, "types", ctx) {
                for child in collect_seq_sync(&types_val, ctx) {
                    if let Some(result) = walk_typenode(&child, ctx, f) {
                        return Some(result);
                    }
                }
            }
        }

        // ── Record: `fields@Child: [Map String TypeNode]` ─────────────────────
        "TypeNode.Record" => {
            for child in get_map_values_field(node, "fields", ctx) {
                if let Some(result) = walk_typenode(&child, ctx, f) {
                    return Some(result);
                }
            }
        }

        // ── TypeApplication: `ctor@Child: TypeNode`, `args@Child: [Seq TypeNode]` ──
        "TypeNode.TypeApplication" => {
            // Visit ctor (One role).
            if let Some(ctor_val) = get_payload_field(node, "ctor", ctx) {
                if let Some(result) = walk_typenode(&ctor_val, ctx, f) {
                    return Some(result);
                }
            }
            // Visit args (Seq role).
            if let Some(args_val) = get_payload_field(node, "args", ctx) {
                for child in collect_seq_sync(&args_val, ctx) {
                    if let Some(result) = walk_typenode(&child, ctx, f) {
                        return Some(result);
                    }
                }
            }
        }

        // ── Arrow: `params@Child: [Seq TypeNode]`, `result@Child: TypeNode` ───
        "TypeNode.Arrow" => {
            // Visit params (Seq role).
            if let Some(params_val) = get_payload_field(node, "params", ctx) {
                for child in collect_seq_sync(&params_val, ctx) {
                    if let Some(result) = walk_typenode(&child, ctx, f) {
                        return Some(result);
                    }
                }
            }
            // Visit result (One role).
            if let Some(result_val) = get_payload_field(node, "result", ctx) {
                if let Some(result) = walk_typenode(&result_val, ctx, f) {
                    return Some(result);
                }
            }
        }

        // ── Recursive: `body@Child: TypeNode` ────────────────────────────────
        // `var: String` has no @Child — not a TypeNode child.
        // Walking into `body` visits `TypeNode.RecursiveRef` leaves at recursive positions
        // (finite, acyclic — not the full Recursive node).
        "TypeNode.Recursive" => {
            if let Some(body_val) = get_payload_field(node, "body", ctx) {
                if let Some(result) = walk_typenode(&body_val, ctx, f) {
                    return Some(result);
                }
            }
        }

        // ── Leaf constructors — no @Child fields ─────────────────────────────
        // TypeNode.Int, TypeNode.Float, TypeNode.String, TypeNode.Bool,
        // TypeNode.Absent, TypeNode.Unknown, TypeNode.Never,
        // TypeNode.TypeConstructor, TypeNode.RecursiveRef, TypeNode.TypeVar
        _ => {}
    }

    None
}

// ── Derived walkers ────────────────────────────────────────────────────────────

/// Returns `true` if the TypeNode Value contains any `TypeNode.TypeVar` nodes.
///
/// A `TypeVar` node represents an unsolved inference variable — the TypeNode-level
/// equivalent of `Type::has_inference_vars()` in the Rust enum representation.
///
/// Traverses the tree in pre-order via [`walk_typenode`], short-circuiting as soon as
/// the first `TypeNode.TypeVar` node is found.
///
/// ## Note on levels
///
/// This function answers "does any TypeVar exist anywhere in the tree?" — it does not
/// filter by level. For generalization, use [`typenode_collect_type_vars`] and filter
/// the resulting names against `state.levels` (authoritative mutable level per
/// Kiselyov 2013). Never read the `level` field from the TypeVar payload — it carries
/// the creation-time level (fixed), not the current level after level lowering.
///
/// ## Scope
///
/// Operates on TypeNode Values (equirecursive sprint). Counterpart to `Type::has_inference_vars()`
/// on the Rust `Type` enum. Used by T-1062; will replace the Rust-enum version after S-860 (CheckerType migration).
pub(crate) fn typenode_has_inference_vars(
    node: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> bool {
    walk_typenode(node, ctx, &mut |v| {
        if is_typenode_typevar(v) {
            Some(true)
        } else {
            None
        }
    })
    .unwrap_or(false)
}

/// Collect all `TypeNode.TypeVar` names from a TypeNode Value into a `Vec<String>`.
///
/// Traverses the entire tree (no short-circuit), accumulating every TypeVar name found.
/// Duplicate names are included if the same TypeVar appears at multiple positions
/// (e.g., in both a parameter type and the return type of an Arrow). Callers that need
/// a unique set should deduplicate via `HashSet`.
///
/// ## Name extraction
///
/// Each `TypeNode.TypeVar` carries a `name: String` field in its payload dict (e.g.
/// `"_t42"`). This function extracts that field for each TypeVar node encountered.
/// If the `name` field is missing or does not materialize to a `Value::String`, the node
/// is silently skipped — it cannot contribute a usable name.
///
/// ## Level note (important for generalization)
///
/// Do **not** use the `level` field from the TypeVar payload for generalization decisions.
/// The payload `level` is the creation-time level (fixed at `fresh_type_var()` call time).
/// `InferState.levels` is the authoritative mutable current level (updated by level
/// lowering). Generalization checks `state.levels[name] > enclosing_level` — always use
/// `state.levels`, never the payload `level`.
///
/// ## Scope
///
/// Counterpart to `Type::collect_type_vars()` on the Rust `Type` enum. Used by T-1062;
/// will replace the Rust-enum version after S-860 (CheckerType migration).
pub(crate) fn typenode_collect_type_vars(
    node: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Vec<String> {
    let mut vars: Vec<String> = Vec::new();

    // walk_typenode short-circuits on Some(_) — we never return Some, so the walk is
    // always exhaustive. The closure collects into `vars` via a mutable capture.
    walk_typenode::<_, ()>(node, ctx, &mut |v| {
        if is_typenode_typevar(v) {
            // Extract the `name` field from the TypeVar payload.
            if let Some(name_val) = get_payload_field(v, "name", ctx) {
                if let Some(name_str) = extract_string_value(&name_val) {
                    vars.push(name_str);
                }
            }
        }
        None // Never short-circuit — always exhaustive.
    });

    vars
}

/// Collect all `TypeNode.TypeVar` names from a TypeNode Value into a `HashSet<String>`.
///
/// Like [`typenode_collect_type_vars`] but deduplicates automatically via `HashSet`.
/// Prefer this over the `Vec` variant when membership testing is needed (e.g., occurs
/// checks, generalization filtering).
pub(crate) fn typenode_collect_type_vars_set(
    node: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> HashSet<String> {
    let mut vars: HashSet<String> = HashSet::new();

    walk_typenode::<_, ()>(node, ctx, &mut |v| {
        if is_typenode_typevar(v) {
            if let Some(name_val) = get_payload_field(v, "name", ctx) {
                if let Some(name_str) = extract_string_value(&name_val) {
                    vars.insert(name_str);
                }
            }
        }
        None
    });

    vars
}

// ── String extraction helper ───────────────────────────────────────────────────

/// Extract a Rust `String` from a `Value::String`.
///
/// Returns `None` for non-string Values. Used internally to read `name` fields from
/// TypeVar and TypeConstructor payloads.
fn extract_string_value(val: &Value) -> Option<String> {
    match val {
        Value::String { source, start, end } => Some(source[*start..*end].to_string()),
        // Annotated wrapper — unwrap transparently.
        Value::Annotated { inner, .. } => extract_string_value(inner),
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for the helper functions that operate on Values directly
    // (without EvalContext) can be written here. Tests for walk_typenode and the
    // derived walkers require a type-stage EvalContext (which in turn requires
    // the prelude to have been compiled). Those are integration-level tests added
    // in typecheck_tests.rs once the infrastructure is available.

    #[test]
    fn test_typenode_tag_plain_variant() {
        let v = Value::Variant {
            tag: "TypeNode.Int".to_string(),
            payload: None,
        };
        assert_eq!(typenode_tag(&v), Some("TypeNode.Int"));
    }

    #[test]
    fn test_typenode_tag_annotated_variant() {
        let inner = Value::Variant {
            tag: "TypeNode.TypeVar".to_string(),
            payload: None,
        };
        let annotated = Value::Annotated {
            inner: Box::new(inner),
            annotation: Box::new(Value::Dict(indexmap::IndexMap::new())),
        };
        assert_eq!(typenode_tag(&annotated), Some("TypeNode.TypeVar"));
    }

    #[test]
    fn test_typenode_tag_non_variant() {
        let v = Value::Int(42);
        assert_eq!(typenode_tag(&v), None);
    }

    #[test]
    fn test_is_typenode_typevar_true() {
        let v = Value::Variant {
            tag: "TypeNode.TypeVar".to_string(),
            payload: None,
        };
        assert!(is_typenode_typevar(&v));
    }

    #[test]
    fn test_is_typenode_typevar_false_for_int() {
        let v = Value::Variant {
            tag: "TypeNode.Int".to_string(),
            payload: None,
        };
        assert!(!is_typenode_typevar(&v));
    }

    #[test]
    fn test_is_typenode_typevar_false_for_non_variant() {
        let v = Value::Int(99);
        assert!(!is_typenode_typevar(&v));
    }

    #[test]
    fn test_extract_string_value_ok() {
        let s: std::rc::Rc<str> = "hello".into();
        let v = Value::String {
            source: s,
            start: 0,
            end: 5,
        };
        assert_eq!(extract_string_value(&v), Some("hello".to_string()));
    }

    #[test]
    fn test_extract_string_value_none_for_int() {
        let v = Value::Int(42);
        assert_eq!(extract_string_value(&v), None);
    }
}
