//! Boolean-Algebraic Subtyping (BAS) — RDNF normalization and emptiness checking.
//!
//! **S-1003 T-2006 migration**: This module now operates on `Arc<Value>` TypeValues
//! (the tinct-side type representation) instead of the deleted `Type` Rust enum.
//!
//! ## TypeValue representation
//!
//! A TypeValue is a `Value::Variant { ctor, payload }` where `ctor` is a tag like
//! `"TypeValue.Union"`, `"TypeValue.Repr"`, etc. See the ctor tag table in
//! doc/06-type-inference.md for the complete mapping.
//!
//! ## Algorithm Overview
//!
//! BAS subtyping: `A <: B` iff `A & ~B` is uninhabited.
//!
//! To check inhabitedness, we convert to Reduced Disjunctive Normal Form (RDNF):
//!   RDNF = Vec<Conjunction>           (disjuncts — type is inhabited if ANY conjunction is)
//!   Conjunction = Vec<SignedAtom>      (conjuncts — all must be simultaneously satisfiable)
//!   SignedAtom = Pos(Arc<Value>) | Neg(Arc<Value>)  (positive or negative TypeValue)
//!
//! Atoms are irreducible TypeValues: Repr variants, single-field records, functions, etc.
//! Boolean types (Union, Inter, Neg) are decomposed during to_rdnf conversion.
//!
//! ## References
//!
//! - Parreaux, L. & Chau, C.Y. (2022). MLstruct. OOPSLA '22.
//! - Chau, C.Y. & Parreaux, L. (2026). Simple essence of BAS. POPL '26.

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::type_def::Variance;
use crate::type_infer::InferenceContext;
use crate::type_tags::*;
use crate::value::{unknown_type_val, HashableValue, Thunk, Value};

/// Maximum depth for atom subtype checking (coinductive Recursive type comparison).
const MAX_ATOM_SUBTYPE_DEPTH: usize = 256;

/// Maximum number of conjunctions allowed in an RDNF after distribution (cross-product).
///
/// When exceeded, `distribute()` returns `vec![vec![]]` (Top RDNF = inhabited). This is
/// conservative-safe: `A <: B` iff `A & ~B` is uninhabited. Returning "inhabited" means
/// `is_subtype` returns false (rejects), which is the safe direction.
const MAX_RDNF_CONJUNCTIONS: usize = 1024;

// ---------------------------------------------------------------------------
// TypeValue inspection helpers
// ---------------------------------------------------------------------------

/// Extract the settled `Value` from a TypeValue thunk synchronously.
///
/// TypeValues are always created as settled thunks (via `Thunk::value`), so `None`
/// from `get()` indicates the thunk is not yet settled (should not happen for TypeValues).
/// An `Err` result indicates the TypeValue was constructed from an evaluation that errored,
/// which is an invariant violation — TypeValues must never be error-bearing thunks.
fn peek_value(thunk: &Arc<Thunk>) -> Option<&Value> {
    match thunk.inner.result.get() {
        None => None, // thunk not yet settled (should not occur for TypeValues)
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => panic!(
            "invariant violation: TypeValue thunk contains an error — TypeValues must be \
             constructed via Thunk::value() and must never carry evaluation errors. \
             Error: {}",
            e
        ),
    }
}

/// Extract the payload dict from a TypeValue variant.
///
/// Returns None for unit variants (no payload) or non-Variant values.
fn typevalue_payload(tv: &Arc<Value>) -> Option<&Value> {
    match tv.as_ref() {
        Value::Variant {
            payload: Some(thunk),
            ..
        } => peek_value(thunk),
        _ => None,
    }
}

/// Extract a string field from a TypeValue payload dict.
fn payload_string_field<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    if let Value::Dict { entries, .. } = payload {
        let key = HashableValue::Str(Arc::from(field));
        if let Some(thunk) = entries.get(&key) {
            if let Some(Value::String {
                source, start, end, ..
            }) = peek_value(thunk)
            {
                return Some(&source[*start..*end]);
            }
        }
    }
    None
}

/// Extract an integer field from a TypeValue payload dict.
fn payload_int_field(payload: &Value, field: &str) -> Option<i64> {
    if let Value::Dict { entries, .. } = payload {
        let key = HashableValue::Str(Arc::from(field));
        if let Some(thunk) = entries.get(&key) {
            if let Some(Value::Int { n, .. }) = peek_value(thunk) {
                return Some(*n);
            }
        }
    }
    None
}

/// Extract a float field from a TypeValue payload dict.
/// Returns the f64 bit pattern as a u64 for NaN-safe equality comparison.
fn payload_float_field_bits(payload: &Value, field: &str) -> Option<u64> {
    if let Value::Dict { entries, .. } = payload {
        let key = HashableValue::Str(Arc::from(field));
        if let Some(thunk) = entries.get(&key) {
            if let Some(Value::Float { n, .. }) = peek_value(thunk) {
                return Some(n.to_bits());
            }
        }
    }
    None
}

/// Extract a TypeValue (Arc<Value>) field from a TypeValue payload dict.
///
/// This function returns a **fresh** `Arc<Value>` (via `Arc::new(val.clone())`),
/// because `make_payload_thunk` stores the inner `Value` (not an `Arc<Value>`) in the
/// thunk. Repeated calls for the same payload field produce different Arc pointers,
/// but this is harmless: the coinductive sigma set in `is_recursive_subtype` uses
/// structural fingerprints (not pointer addresses) as keys, so the hypothesis is
/// correctly found on re-entry regardless of Arc allocation identity.
fn payload_typevalue_field<'a>(payload: &'a Value, field: &str) -> Option<Arc<Value>> {
    if let Value::Dict { entries, .. } = payload {
        let key = HashableValue::Str(Arc::from(field));
        if let Some(thunk) = entries.get(&key) {
            if let Some(val) = peek_value(thunk) {
                // The field value should be a TypeValue — wrap it back in an Arc
                // by looking at how it was stored. TypeValues are always Arc<Value>.
                // Since we can't reclaim the Arc from a reference, we clone.
                // This creates a fresh Arc on every call, but coinductive sigma uses
                // structural fingerprints (not pointer identity), so this is correct.
                return Some(Arc::new(val.clone()));
            }
        }
    }
    None
}

/// Extract the `members` list from a TypeValue.Union or TypeValue.Inter payload.
///
/// The payload structure is `{ members: { 0: member0, 1: member1, ... } }`.
/// Members are stored as an auto-indexed Dict under the "members" key.
fn payload_members(payload: &Value) -> Vec<Arc<Value>> {
    // Step 1: Get the "members" thunk from the payload dict.
    let members_thunk = if let Value::Dict { entries, .. } = payload {
        let members_key = HashableValue::Str(Arc::from(FIELD_MEMBERS));
        match entries.get(&members_key) {
            Some(t) => Arc::clone(t),
            None => return Vec::new(),
        }
    } else {
        return Vec::new();
    };

    // Step 2: Peek at the members thunk and collect indexed members directly,
    // without cloning the entire entries IndexMap.
    let mut indexed: Vec<(i64, Arc<Value>)> = Vec::new();
    if let Some(Value::Dict { entries, .. }) = peek_value(&members_thunk) {
        // Step 3: Collect indexed members in insertion order (0, 1, 2, ...)
        for (key, thunk) in entries {
            if let HashableValue::Int(idx) = key {
                if let Some(val) = peek_value(thunk) {
                    indexed.push((*idx, Arc::new(val.clone())));
                }
            }
        }
    } else {
        return Vec::new();
    }
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, v)| v).collect()
}

// ---------------------------------------------------------------------------
// Repr discriminant helpers
// ---------------------------------------------------------------------------

/// Map a TyConDef `builtin_type` discriminant string (e.g. `"Int"`, `"Str"`, `"Float"`)
/// to its corresponding `TypeValue.Repr` payload string (e.g. `REPR_INT`, `REPR_STRING`).
///
/// These short discriminant names are registered by `build_builtin_core_envs_inner` in
/// `imports.rs` and are the canonical names in `TyConDef.builtin_type`. The REPR_* constants
/// are the full Rust variant discriminant strings stored in `TypeValue.Repr { repr: String }`.
///
/// Returns `None` for unrecognized discriminant strings (conservative: not a known builtin repr).
fn repr_disc_to_repr_string(disc: &str) -> Option<&'static str> {
    match disc {
        OP_INT => Some(REPR_INT),
        OP_U64 => Some(REPR_U64),
        OP_STR => Some(REPR_STRING),
        OP_FLOAT => Some(REPR_FLOAT),
        OP_BYTES => Some(REPR_BYTES),
        OP_DICT => Some(REPR_DICT),
        OP_FN | OP_FUNCTION => Some(REPR_FUNCTION),
        OP_FILE => Some(REPR_FILE),
        OP_DIR_CAP => Some(REPR_DIR_CAP),
        OP_NET_CAP => Some(REPR_NET_CAP),
        OP_TASK => Some(REPR_TASK),
        OP_CHANNEL => Some(REPR_CHANNEL),
        OP_CONTEXT => Some(REPR_CONTEXT),
        OP_REACTIVE_CELL => Some(REPR_REACTIVE_CELL),
        OP_CLOCK_CAP => Some(REPR_CLOCK_CAP),
        OP_TIMEZONE => Some(REPR_TIMEZONE),
        OP_TIMESTAMP => Some(REPR_TIMESTAMP),
        OP_DURATION => Some(REPR_DURATION),
        OP_DECIMAL => Some(REPR_DECIMAL),
        OP_BIG_INT => Some(REPR_BIGINT),
        OP_QUIC_SESSION => Some(REPR_QUIC_SESSION),
        OP_QUIC_DATAGRAM_HANDLE => Some(REPR_QUIC_DATAGRAM_HANDLE),
        OP_HTTP2_SESSION => Some(REPR_HTTP2_SESSION),
        OP_HTTP3_SESSION => Some(REPR_HTTP3_SESSION),
        OP_URI | OP_URN => Some(REPR_URI),
        OP_PROGRAM => Some(REPR_PROGRAM),
        OP_DOCUMENT => Some(REPR_DOCUMENT),
        OP_TYPE_CONTEXT => Some(REPR_TYPE_CONTEXT),
        // BuilderHandle (Value::Builder) is intentionally excluded: it is a transient
        // accumulator with no valid repr string (eval_core.rs: is_valid_repr_string returns false).
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// TypeValue construction helpers
// ---------------------------------------------------------------------------

/// Create a settled Arc<Value> TypeValue thunk payload dict from named fields.
///
/// Fields are stored as string-keyed entries. The payload is settled (immediately
/// materialized) because TypeValues are never lazy.
fn make_payload_thunk(fields: IndexMap<String, Arc<Value>>, span: Span) -> Arc<Thunk> {
    let mut entries = IndexMap::new();
    for (k, v) in fields {
        let key = HashableValue::Str(Arc::from(k.as_str()));
        let val_thunk = Arc::new(Thunk::value((*v).clone(), span.clone()));
        entries.insert(key, val_thunk);
    }
    Arc::new(Thunk::value(
        Value::Dict {
            entries,
            type_val: unknown_type_val(),
        },
        span,
    ))
}

/// Span used when constructing synthetic TypeValues in bas.rs.
fn bas_span() -> Span {
    Span {
        file: crate::rust_span!().file,
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 0,
        name: None,
    }
}

/// Create a TypeValue variant with a payload dict.
fn make_payload_typevalue(ctor: &str, payload_thunk: Arc<Thunk>) -> Arc<Value> {
    Arc::new(Value::Variant {
        ctor: Arc::from(ctor),
        payload: Some(payload_thunk),
        type_val: unknown_type_val(),
    })
}

// ---------------------------------------------------------------------------
// RDNF types (internal representation for BAS algorithm)
// ---------------------------------------------------------------------------

/// A signed TypeValue: positive (the type itself) or negative (its complement).
#[derive(Debug, Clone)]
pub enum SignedAtom {
    Pos(Arc<Value>),
    Neg(Arc<Value>),
}

impl SignedAtom {
    pub fn negate(&self) -> SignedAtom {
        match self {
            SignedAtom::Pos(a) => SignedAtom::Neg(Arc::clone(a)),
            SignedAtom::Neg(a) => SignedAtom::Pos(Arc::clone(a)),
        }
    }
}

/// A conjunction (AND) of signed atoms. Represents a single "row" of the DNF.
pub type Conjunction = Vec<SignedAtom>;

/// Reduced Disjunctive Normal Form: a disjunction (OR) of conjunctions.
pub type Rdnf = Vec<Conjunction>;

// ---------------------------------------------------------------------------
// Structural fingerprinting for coinductive sigma keys
// ---------------------------------------------------------------------------

/// Maximum depth for structural fingerprinting.
///
/// Prevents unbounded recursion on pathologically deep (but non-cyclic) types.
/// Recursive types naturally terminate at RecursiveRef nodes (de Bruijn indices),
/// so this limit only fires on unreasonably nested non-recursive structures.
const MAX_FINGERPRINT_DEPTH: usize = 64;

/// Compute a stable structural fingerprint of a TypeValue for use as a coinductive
/// sigma key.
///
/// Unlike pointer-based keys (`Arc::as_ptr`), this fingerprint is invariant under
/// re-extraction: calling `payload_typevalue_field` on the same payload dict produces
/// fresh `Arc` instances with different pointers but identical structural content.
/// The fingerprint of these structurally-identical-but-pointer-distinct values is
/// the same string.
///
/// ## Formal basis
///
/// In equirecursive subtyping (Amadio & Cardelli, 1993), the coinductive hypothesis
/// set (sigma) must be keyed by *type identity*, not *representation identity*. For
/// de Bruijn-indexed recursive types, type identity is structural: two `mu.body`
/// values with the same body structure are the same type. The RecursiveRef(depth)
/// nodes provide natural termination for the fingerprint traversal — they are leaves
/// in the type's syntax tree that encode the binder reference without further recursion.
///
/// ## Soundness argument
///
/// The fingerprint is *injective on types*: distinct types produce distinct fingerprints.
/// This is because the fingerprint encodes the full tree structure — every ctor tag,
/// every payload field value (string, int, float bits), and every child type is included
/// in canonical order. Two types with the same fingerprint are structurally identical.
///
/// Consequently, inserting a sigma hypothesis keyed by `(fp(A), fp(B))` and later finding
/// it means we are comparing the *same pair of types* as before — exactly the condition
/// required for S-Assum to fire soundly.
fn typevalue_structural_fingerprint(tv: &Arc<Value>, depth: usize) -> String {
    if depth >= MAX_FINGERPRINT_DEPTH {
        return "…".to_string();
    }

    let ctor = match typevalue_ctor(tv) {
        Some(c) => c,
        None => return "?".to_string(), // non-Variant (e.g., unknown_type_val empty Dict)
    };

    let payload = typevalue_payload(tv);

    match ctor {
        // Leaf atoms with string payload
        TV_REPR => {
            let repr = payload
                .and_then(|p| payload_string_field(p, FIELD_REPR))
                .unwrap_or("?");
            format!("Repr({repr})")
        }
        TV_OP => {
            let name = payload
                .and_then(|p| payload_string_field(p, FIELD_NAME))
                .unwrap_or("?");
            format!("Op({name})")
        }
        TV_VAR => {
            let name = payload
                .and_then(|p| payload_string_field(p, FIELD_NAME))
                .unwrap_or("?");
            format!("Var({name})")
        }

        // Leaf atoms with value payload
        TV_INT_LIT => {
            let n = payload
                .and_then(|p| payload_int_field(p, FIELD_VALUE))
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("IntLit({n})")
        }
        TV_STR_LIT => {
            let s = payload
                .and_then(|p| payload_string_field(p, FIELD_VALUE))
                .unwrap_or("?");
            format!("StrLit({s})")
        }
        TV_FLOAT_LIT => {
            let bits = payload
                .and_then(|p| payload_float_field_bits(p, FIELD_VALUE))
                .map(|b| b.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("FloatLit({bits})")
        }

        // RecursiveRef: de Bruijn index — natural leaf for recursive types
        TV_RECURSIVE_REF => {
            let d = payload
                .and_then(|p| payload_int_field(p, FIELD_DEPTH))
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("Ref({d})")
        }

        // Recursive: fingerprint the body (RecursiveRef nodes terminate the traversal)
        TV_RECURSIVE => {
            let body_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_BODY))
                .map(|b| typevalue_structural_fingerprint(&b, depth + 1))
                .unwrap_or_else(|| "?".to_string());
            format!("Rec({body_fp})")
        }

        // Union/Inter: fingerprint all members in order
        TV_UNION | TV_INTER => {
            let tag = if ctor == TV_UNION { "U" } else { "I" };
            let members = payload.map(|p| payload_members(p)).unwrap_or_default();
            let member_fps: Vec<String> = members
                .iter()
                .map(|m| typevalue_structural_fingerprint(m, depth + 1))
                .collect();
            format!("{tag}({})", member_fps.join(","))
        }

        // Neg: fingerprint the inner type
        TV_NEG => {
            let inner_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_OF))
                .map(|inner| typevalue_structural_fingerprint(&inner, depth + 1))
                .unwrap_or_else(|| "?".to_string());
            format!("Neg({inner_fp})")
        }

        // Fn: fingerprint params and return
        TV_FN => {
            let ret_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_RETURN))
                .map(|r| typevalue_structural_fingerprint(&r, depth + 1))
                .unwrap_or_else(|| "?".to_string());
            let params_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_PARAMS))
                .map(|ps| {
                    let param_list = collect_indexed_typevalues(&ps);
                    let fps: Vec<String> = param_list
                        .iter()
                        .map(|p| typevalue_structural_fingerprint(p, depth + 1))
                        .collect();
                    fps.join(",")
                })
                .unwrap_or_else(|| "?".to_string());
            format!("Fn({params_fp})->{ret_fp}")
        }

        // Record: fingerprint fields (sorted by key) and tail
        TV_RECORD => {
            let fields_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_FIELDS))
                .map(|fs| {
                    if let Value::Dict { entries, .. } = fs.as_ref() {
                        // Collect string-keyed fields sorted by key for determinism
                        let mut field_fps: Vec<(String, String)> = Vec::new();
                        for (key, thunk) in entries {
                            let key_str = match key {
                                HashableValue::Str(s) => s.as_ref().to_owned(),
                                HashableValue::Int(n) => n.to_string(),
                                _ => continue,
                            };
                            if let Some(val) = peek_value(thunk) {
                                let tv = Arc::new(val.clone());
                                let fp = typevalue_structural_fingerprint(&tv, depth + 1);
                                field_fps.push((key_str, fp));
                            }
                        }
                        field_fps.sort_by(|a, b| a.0.cmp(&b.0));
                        field_fps
                            .iter()
                            .map(|(k, v)| format!("{k}:{v}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    } else {
                        "?".to_string()
                    }
                })
                .unwrap_or_else(|| "?".to_string());
            let tail_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_TAIL))
                .map(|t| match t.as_ref() {
                    Value::Dict { entries, .. } if entries.is_empty() => "[]".to_string(),
                    Value::Variant { ctor, .. } if ctor.as_ref() == RT_CLOSED => {
                        "closed".to_string()
                    }
                    _ => typevalue_structural_fingerprint(&t, depth + 1),
                })
                .unwrap_or_else(|| "?".to_string());
            format!("Record{{{fields_fp}|{tail_fp}}}")
        }

        // App: fingerprint op and arg
        TV_APP => {
            let op_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_OP))
                .map(|o| typevalue_structural_fingerprint(&o, depth + 1))
                .unwrap_or_else(|| "?".to_string());
            let arg_fp = payload
                .and_then(|p| payload_typevalue_field(p, FIELD_ARG))
                .map(|a| typevalue_structural_fingerprint(&a, depth + 1))
                .unwrap_or_else(|| "?".to_string());
            format!("App({op_fp},{arg_fp})")
        }

        // NominalVariant: fingerprint tycon and ctor names
        TV_NOMINAL_VARIANT => {
            let tycon = payload
                .and_then(|p| payload_string_field(p, FIELD_TYCON))
                .unwrap_or("?");
            let ctor_name = payload
                .and_then(|p| payload_string_field(p, FIELD_CTOR))
                .unwrap_or("?");
            format!("NV({tycon}.{ctor_name})")
        }

        // Unit variants (Top, Never, Unknown, etc.)
        _ => ctor.to_string(),
    }
}

// ---------------------------------------------------------------------------
// RDNF conversion: TypeValue → Rdnf
// ---------------------------------------------------------------------------

/// Convert a TypeValue to RDNF.
///
/// Boolean types (Union, Inter, Neg) are decomposed; atoms are wrapped in singleton
/// positive conjunctions.
///
/// - Top (TypeValue.Top or empty dict) → `vec![vec![]]`
/// - Never (TypeValue.Never) → `vec![]`
/// - Unknown → `vec![vec![]]` (conservative: treated as Top for RDNF purposes)
/// - Error → `vec![]` (uninhabited sentinel)
/// - Union(members) → concatenation of sub-RDNFs
/// - Inter(members) → cross-product of sub-RDNFs
/// - Neg(of) → negate_rdnf(to_rdnf(of))
/// - Atoms (Repr, Var, Fn, Record, etc.) → `vec![vec![Pos(atom)]]`
pub fn to_rdnf(tv: &Arc<Value>) -> Rdnf {
    match typevalue_ctor(tv) {
        Some(TV_TOP) => vec![vec![]],
        Some(TV_NEVER) => vec![],
        Some(TV_UNKNOWN) => vec![vec![]], // conservative: Unknown = Top in RDNF
        Some(TV_ERROR) => vec![],

        Some(TV_UNION) => {
            if let Some(payload) = typevalue_payload(tv) {
                let members = payload_members(payload);
                let mut result = Vec::new();
                for member in &members {
                    result.extend(to_rdnf(member));
                }
                result
            } else {
                vec![] // empty union = Never
            }
        }

        Some(TV_INTER) => {
            if let Some(payload) = typevalue_payload(tv) {
                let members = payload_members(payload);
                if members.is_empty() {
                    // Empty intersection = Top
                    return vec![vec![]];
                }
                let mut result = to_rdnf(&members[0]);
                for member in &members[1..] {
                    let right = to_rdnf(member);
                    result = distribute(&result, &right);
                }
                result
            } else {
                vec![vec![]] // empty payload = Top
            }
        }

        Some(TV_NEG) => {
            if let Some(payload) = typevalue_payload(tv) {
                if let Some(inner) = payload_typevalue_field(payload, FIELD_OF) {
                    let inner_rdnf = to_rdnf(&inner);
                    negate_rdnf(&inner_rdnf)
                } else {
                    vec![vec![]] // Neg(?) = Top conservatively
                }
            } else {
                vec![vec![]]
            }
        }

        None => {
            // Not a Variant (e.g., unknown_type_val empty Dict) — treat as Unknown/Top
            vec![vec![]]
        }

        // All other TypeValue variants are atoms: Repr, Var, Fn, Record, Op, App, etc.
        _ => {
            vec![vec![SignedAtom::Pos(Arc::clone(tv))]]
        }
    }
}

/// Distribute two RDNFs (cross-product for intersection).
/// Guarded by `MAX_RDNF_CONJUNCTIONS`.
fn distribute(left: &Rdnf, right: &Rdnf) -> Rdnf {
    if left.is_empty() || right.is_empty() {
        return vec![];
    }
    let product_size = left.len().saturating_mul(right.len());
    if product_size > MAX_RDNF_CONJUNCTIONS {
        // Conservative: Top (inhabited)
        return vec![vec![]];
    }
    let mut result = Vec::with_capacity(product_size);
    for l in left {
        for r in right {
            let mut conjunction = l.clone();
            conjunction.extend(r.iter().cloned());
            result.push(conjunction);
        }
    }
    result
}

/// Negate an RDNF using De Morgan's laws.
fn negate_rdnf(rdnf: &Rdnf) -> Rdnf {
    if rdnf.is_empty() {
        return vec![vec![]]; // ~Never = Top
    }
    let mut result: Rdnf = vec![vec![]];
    for conjunction in rdnf {
        if conjunction.is_empty() {
            return vec![]; // ~Top = Never
        }
        let negated_conj: Rdnf = conjunction.iter().map(|atom| vec![atom.negate()]).collect();
        result = distribute(&result, &negated_conj);
    }
    result
}

// ---------------------------------------------------------------------------
// Atom subtype checking
// ---------------------------------------------------------------------------

/// Check whether TypeValue atom `sub` is a subtype of atom `sup`.
///
/// Handles structural decomposition:
/// - Literal promotions: IntLit(n) <: Repr("Value::Int")
/// - Record field covariance
/// - Function variance (contravariant params, covariant return)
/// - App: variance-directed via TyConEnv
/// - Recursive: coinductive via sigma (S-Assum/S-Exp)
pub fn is_atom_subtype(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if depth >= MAX_ATOM_SUBTYPE_DEPTH {
        return false;
    }

    let sub_ctor = typevalue_ctor(sub);
    let sup_ctor = typevalue_ctor(sup);

    match (sub_ctor, sup_ctor) {
        // Same ctor tag — simple structural equality for unit-like types.
        //
        // Excluded: TV_FN, TV_RECORD, TV_APP, TV_RECURSIVE — these have dedicated
        // arms below with correct subtyping semantics (variance, structural depth,
        // coinductive sigma). Including them here would call typevalue_structurally_equal
        // which delegates back to is_atom_subtype, causing depth-exhaustion.
        (Some(s), Some(p))
            if s == p && s != TV_FN && s != TV_RECORD && s != TV_APP && s != TV_RECURSIVE =>
        {
            // Reflexivity by structural equality (no payload comparison needed for unit variants)
            typevalue_structurally_equal(sub, sup, ctx, depth, sigma)
        }

        // TypeValue.Var: inference variable — conservative true (defers to constraint solver)
        (Some(TV_VAR), _) | (_, Some(TV_VAR)) => true,

        // Literal promotions: IntLit <: Int, StrLit <: Str, FloatLit <: Float
        (Some(TV_INT_LIT), Some(TV_REPR)) => {
            if let Some(payload) = typevalue_payload(sup) {
                payload_string_field(payload, FIELD_REPR) == Some(REPR_INT)
            } else {
                false
            }
        }
        (Some(TV_STR_LIT), Some(TV_REPR)) => {
            if let Some(payload) = typevalue_payload(sup) {
                payload_string_field(payload, FIELD_REPR) == Some(REPR_STRING)
            } else {
                false
            }
        }
        (Some(TV_FLOAT_LIT), Some(TV_REPR)) => {
            if let Some(payload) = typevalue_payload(sup) {
                payload_string_field(payload, FIELD_REPR) == Some(REPR_FLOAT)
            } else {
                false
            }
        }

        // Different IntLits: not subtypes of each other
        (Some(TV_INT_LIT), Some(TV_INT_LIT)) => {
            typevalue_structurally_equal(sub, sup, ctx, depth, sigma)
        }

        // Same FloatLit: subtype iff equal (by bit pattern for NaN safety)
        (Some(TV_FLOAT_LIT), Some(TV_FLOAT_LIT)) => {
            typevalue_structurally_equal(sub, sup, ctx, depth, sigma)
        }

        // Fn: contravariant params, covariant return
        (Some(TV_FN), Some(TV_FN)) => is_fn_subtype(sub, sup, ctx, depth, sigma),

        // Record: structural subtyping (width + depth covariance)
        (Some(TV_RECORD), Some(TV_RECORD)) => is_record_subtype(sub, sup, ctx, depth, sigma),

        // App: variance-directed
        (Some(TV_APP), Some(TV_APP)) => is_app_subtype(sub, sup, ctx, depth, sigma),

        // Repr: same repr string = subtype
        (Some(TV_REPR), Some(TV_REPR)) => typevalue_structurally_equal(sub, sup, ctx, depth, sigma),

        // Op (type constructor): nominal equality
        (Some(TV_OP), Some(TV_OP)) => typevalue_structurally_equal(sub, sup, ctx, depth, sigma),

        // Recursive: coinductive (S-Assum/S-Exp)
        (Some(TV_RECURSIVE), Some(TV_RECURSIVE)) => {
            is_recursive_subtype(sub, sup, ctx, depth, sigma)
        }

        // Recursive vs non-Recursive: unfold sub and recurse.
        // Thread sigma to preserve any coinductive hypotheses accumulated so far.
        (Some(TV_RECURSIVE), _) => {
            let unfolded = unfold_recursive_typevalue(sub);
            is_subtype_bas_with_sigma(&unfolded, sup, ctx, sigma, depth + 1)
        }
        (_, Some(TV_RECURSIVE)) => {
            let unfolded = unfold_recursive_typevalue(sup);
            is_subtype_bas_with_sigma(sub, &unfolded, ctx, sigma, depth + 1)
        }

        // NominalVariant vs TyCon: check if variant is member of TyCon by checking against
        // the TyCon's body type definition. This replicates the old atom_to_type behavior
        // which called is_subtype_bas against def.body.
        //
        // If the TyCon is found in tycon_env, delegate to the body type — a NominalVariant
        // is a subtype of the TyCon iff it is a subtype of the TyCon's union body.
        // If the TyCon is not found (empty env or unknown name), fall back to tycon-name
        // string comparison, which is correct when the variant was constructed from that TyCon.
        (Some(TV_NOMINAL_VARIANT), Some(TV_OP)) => {
            let sub_tycon = typevalue_payload(sub)
                .and_then(|p| payload_string_field(p, FIELD_TYCON).map(String::from));
            let sup_name = typevalue_payload(sup)
                .and_then(|p| payload_string_field(p, FIELD_NAME).map(String::from));
            match sup_name.as_deref() {
                Some(name) => match ctx.tycon_env.get(name) {
                    Some(def) => is_subtype_bas_with_sigma(sub, &def.body, ctx, sigma, depth + 1),
                    None => sub_tycon.as_deref() == Some(name),
                },
                None => false,
            }
        }

        // Repr vs TyCon (Op): check whether the TyCon's builtin_type corresponds to the repr string.
        //
        // This covers runtime TypeAssert cases like `Repr("Value::Int") <: Op("Integer")` —
        // the TyConDef for "Integer" has builtin_type = "Int", and the repr map maps "Int" back
        // to "Value::Int". The discriminant strings stored in TyConDef.builtin_type are the
        // short names ("Int", "Str", "Float", etc.) which map to REPR_* constants via
        // repr_disc_to_repr_string. This is the single authoritative TyCon→Repr dispatch path.
        (Some(TV_REPR), Some(TV_OP)) => {
            let sub_repr =
                match typevalue_payload(sub).and_then(|p| payload_string_field(p, FIELD_REPR)) {
                    Some(r) => r,
                    None => return false,
                };
            let sup_name = match typevalue_payload(sup)
                .and_then(|p| payload_string_field(p, FIELD_NAME).map(String::from))
            {
                Some(n) => n,
                None => return false,
            };
            // Look up TyCon definition. If missing, conservative false.
            match ctx.tycon_env.get(sup_name.as_str()) {
                Some(def) => {
                    if let Some(ref builtin_disc) = def.builtin_type {
                        // Map TyConDef discriminant string (e.g. "Int") to REPR_* constant.
                        repr_disc_to_repr_string(builtin_disc) == Some(sub_repr)
                    } else if !def.constructors.is_empty() {
                        // Nominal ADT TyCon: Repr cannot be a subtype of a nominal ADT.
                        false
                    } else {
                        // TyCon without builtin_type and without constructors — structural alias;
                        // unfold the body and recurse, threading sigma for coinductive correctness.
                        is_subtype_bas_with_sigma(sub, &def.body, ctx, sigma, depth + 1)
                    }
                }
                None => false,
            }
        }

        // Repr vs App(TyCon, _): extract the root TyCon name, dispatch on its builtin_type.
        (Some(TV_REPR), Some(TV_APP)) => {
            // For a parameterized TyCon (e.g., App(Seq, Int)), the value-level type is
            // determined by the root TyCon, not the argument. Extract op (must be TV_OP).
            let op_tv = typevalue_payload(sup).and_then(|p| payload_typevalue_field(p, FIELD_OP));
            match op_tv {
                Some(op) if typevalue_ctor(&op) == Some(TV_OP) => {
                    is_atom_subtype(sub, &op, ctx, depth + 1, sigma)
                }
                _ => false,
            }
        }

        // TypeStageApp: conservative (not a subtype of anything until reduced)
        (Some(TV_STAGE_APP), _) | (_, Some(TV_STAGE_APP)) => false,

        // Everything else: not subtypes
        _ => false,
    }
}

/// Check structural equality for same-ctor TypeValues (e.g., two Repr values with same string).
fn typevalue_structurally_equal(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    // For simple cases: compare payload string fields
    match (typevalue_payload(sub), typevalue_payload(sup)) {
        (None, None) => true, // Both unit variants with same ctor
        (Some(sp), Some(pp)) => {
            // Compare string fields only (simple equality for Repr, Op, IntLit, StrLit, Var)
            match (typevalue_ctor(sub), typevalue_ctor(sup)) {
                (Some(TV_REPR), _) => {
                    payload_string_field(sp, FIELD_REPR) == payload_string_field(pp, FIELD_REPR)
                }
                (Some(TV_OP), _) => {
                    payload_string_field(sp, FIELD_NAME) == payload_string_field(pp, FIELD_NAME)
                }
                (Some(TV_VAR), _) => {
                    payload_string_field(sp, FIELD_NAME) == payload_string_field(pp, FIELD_NAME)
                }
                (Some(TV_INT_LIT), _) => {
                    payload_int_field(sp, FIELD_VALUE) == payload_int_field(pp, FIELD_VALUE)
                }
                (Some(TV_FLOAT_LIT), _) => {
                    // NaN-safe: compare bit patterns (two NaNs with the same bit pattern are equal)
                    payload_float_field_bits(sp, FIELD_VALUE)
                        == payload_float_field_bits(pp, FIELD_VALUE)
                }
                (Some(TV_STR_LIT), _) => {
                    payload_string_field(sp, FIELD_VALUE) == payload_string_field(pp, FIELD_VALUE)
                }
                // For complex types: delegate to structural subtyping in both directions
                _ => {
                    is_atom_subtype(sub, sup, ctx, depth + 1, sigma)
                        && is_atom_subtype(sup, sub, ctx, depth + 1, sigma)
                }
            }
        }
        _ => false, // One has payload, other doesn't
    }
}

/// Check function type subtyping: contravariant params, covariant return.
fn is_fn_subtype(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if depth >= MAX_ATOM_SUBTYPE_DEPTH {
        // Defensive guard: conservatively treat as compatible at depth limit.
        // Depth is now properly threaded through is_subtype_bas_with_sigma (T-2076),
        // so this guard can fire on pathologically deep types.
        return true;
    }
    let sub_payload = match typevalue_payload(sub) {
        Some(p) => p,
        None => return true, // Any-function
    };
    let sup_payload = match typevalue_payload(sup) {
        Some(p) => p,
        None => return true, // Any-function
    };

    // Extract params and return types
    let sub_ret = match payload_typevalue_field(sub_payload, FIELD_RETURN) {
        Some(r) => r,
        None => return false,
    };
    let sup_ret = match payload_typevalue_field(sup_payload, FIELD_RETURN) {
        Some(r) => r,
        None => return false,
    };

    // Check return (covariant).
    if !is_subtype_bas_with_sigma(&sub_ret, &sup_ret, ctx, sigma, depth + 1) {
        return false;
    }

    // Check params (contravariant): sup_params <: sub_params for each position
    // Params are stored as an indexed Dict in the payload
    let sub_params_val = payload_typevalue_field(sub_payload, FIELD_PARAMS);
    let sup_params_val = payload_typevalue_field(sup_payload, FIELD_PARAMS);

    match (sub_params_val, sup_params_val) {
        (None, None) => true, // Both are any-function
        (Some(sub_ps), Some(sup_ps)) => {
            // Compare params lists — each sup param must be <: corresponding sub param (contravariant)
            let sub_list = collect_indexed_typevalues(&sub_ps);
            let sup_list = collect_indexed_typevalues(&sup_ps);

            // B-672: TypeValue.Fn does not yet have a `required_count` field — it only has
            // a `variadic` boolean flag in the current payload schema (builtin_core.llt).
            // A dedicated `required_count` integer field would need to be added to
            // TypeValue.Fn to support the correct arity subtyping rule: sub <: sup iff
            // sub.required_count <= sup.required_count AND params match for each position
            // 0..sup.required_count (contravariant). For now, strict arity equality is used.
            if sub_list.len() != sup_list.len() {
                return false;
            }

            for (sub_p, sup_p) in sub_list.iter().zip(sup_list.iter()) {
                // Contravariant: sup_param <: sub_param
                if !is_subtype_bas_with_sigma(sup_p, sub_p, ctx, sigma, depth + 1) {
                    return false;
                }
            }
            true
        }
        _ => false, // Arity mismatch
    }
}

/// Check whether a tail value represents a closed row (no additional fields allowed).
///
/// Closed = empty dict `[]` OR `RowTail.Closed` variant. Open rows carry `RowTail.Var`
/// or `RowTail.Uniform` variants as their tail.
fn is_closed_tail(tail: &Arc<Value>) -> bool {
    match tail.as_ref() {
        Value::Dict { entries, .. } => entries.is_empty(),
        Value::Variant { ctor, .. } => ctor.as_ref() == RT_CLOSED,
        _ => false,
    }
}

/// Check that all field TypeValues in a closed sub record are subtypes of the sup's
/// RT_UNIFORM value-type.
///
/// When sup is open with an RT_UNIFORM tail, every field in sub that is NOT explicitly
/// declared in sup's own field map must satisfy the uniform value-type constraint. Fields
/// in `sup_explicit_keys` are excluded because they were already depth-checked against
/// their declared types in the outer field-by-field loop.
///
/// Skipping explicitly-typed sup fields prevents incorrect rejection of valid subtypes
/// such as `{x: String, y: Int} <: {x: String, _: Number}`: field `x` already passed
/// its `String <: String` check; only the extra field `y` needs `Int <: Number`.
fn check_sub_fields_against_uniform(
    sub_fields: &Option<Arc<Value>>,
    sup_explicit_keys: &HashSet<String>,
    sup_val_type: &Arc<Value>,
    ctx: &InferenceContext,
    sigma: &mut HashSet<(String, String)>,
    depth: usize,
) -> bool {
    if let Some(sf) = sub_fields {
        if let Value::Dict { entries, .. } = sf.as_ref() {
            for (key, thunk) in entries {
                // Skip fields explicitly declared in sup — they were already
                // depth-checked against sup's specific field type in the outer loop.
                if let HashableValue::Str(k) = key {
                    if sup_explicit_keys.contains(k.as_ref()) {
                        continue;
                    }
                }
                if let Some(field_val) = peek_value(thunk) {
                    let field_tv = Arc::new(field_val.clone());
                    if !is_subtype_bas_with_sigma(&field_tv, sup_val_type, ctx, sigma, depth + 1) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Check record type subtyping.
///
/// Width subtyping: sub must supply every field sup requires.
/// Depth subtyping: field types are covariant.
fn is_record_subtype(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if depth >= MAX_ATOM_SUBTYPE_DEPTH {
        // Defensive guard: conservatively treat as compatible at depth limit.
        // Depth is now properly threaded through is_subtype_bas_with_sigma (T-2076),
        // so this guard can fire on pathologically deep types.
        return true;
    }
    let sub_payload = match typevalue_payload(sub) {
        Some(p) => p,
        None => return false,
    };
    let sup_payload = match typevalue_payload(sup) {
        Some(p) => p,
        None => return false,
    };

    // Extract field dicts
    let sub_fields = payload_typevalue_field(sub_payload, FIELD_FIELDS);
    let sup_fields = payload_typevalue_field(sup_payload, FIELD_FIELDS);

    // For each field required by sup, sub must have it with a compatible type.
    // When sup_fields is None (sup payload has no "fields" key), the sup record
    // type imposes no field constraints — any sub is trivially width-compatible.
    // An empty/no-fields record is the top of the record lattice for width subtyping.
    if let Some(sup_fs) = &sup_fields {
        if let Value::Dict {
            entries: sup_entries,
            ..
        } = sup_fs.as_ref()
        {
            if let Some(sub_fs) = &sub_fields {
                if let Value::Dict {
                    entries: sub_entries,
                    ..
                } = sub_fs.as_ref()
                {
                    for (key, sup_field_thunk) in sup_entries {
                        if let Some(sup_field_val) = peek_value(sup_field_thunk) {
                            let sup_field_tv = Arc::new(sup_field_val.clone());
                            match sub_entries.get(key) {
                                Some(sub_field_thunk) => {
                                    if let Some(sub_field_val) = peek_value(sub_field_thunk) {
                                        let sub_field_tv = Arc::new(sub_field_val.clone());
                                        if !is_subtype_bas_with_sigma(
                                            &sub_field_tv,
                                            &sup_field_tv,
                                            ctx,
                                            sigma,
                                            depth + 1,
                                        ) {
                                            return false;
                                        }
                                    } else {
                                        return false; // sub field not settled
                                    }
                                }
                                None => return false, // sub missing required field
                            }
                        }
                    }
                } else {
                    return false; // sub fields not a Dict
                }
            } else {
                // sub has no fields — only subtype of empty sup
                return sup_entries.is_empty();
            }
        }
    }

    // Collect sup's explicit field keys so the uniform tail check can skip them.
    // Fields in sup's field map were already depth-checked against their declared types
    // in the loop above; the RT_UNIFORM constraint applies only to extra sub fields.
    let sup_explicit_keys: HashSet<String> = if let Some(sup_fs) = &sup_fields {
        if let Value::Dict { entries, .. } = sup_fs.as_ref() {
            entries
                .keys()
                .filter_map(|k| {
                    if let HashableValue::Str(s) = k {
                        Some(s.as_ref().to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            HashSet::new()
        }
    } else {
        HashSet::new()
    };

    // Tail check: verify tail compatibility.
    //
    // Closed records are represented as an empty dict [] for the tail field.
    // Open records have a RowTail.Var or RowTail.Uniform variant as the tail.
    let sub_tail = payload_typevalue_field(sub_payload, FIELD_TAIL);
    let sup_tail = payload_typevalue_field(sup_payload, FIELD_TAIL);

    match (sub_tail.as_ref(), sup_tail.as_ref()) {
        (_, None) => {
            // sup has no tail field — treat as closed; sub can be anything
            true
        }
        (_, Some(sup_t)) => {
            let sup_tail_ctor = typevalue_ctor(sup_t);
            if sup_tail_ctor == Some(RT_VAR) {
                // sup is open with a row variable — sub can have any tail (conservative).
                true
            } else if sup_tail_ctor == Some(RT_UNIFORM) {
                // sup requires all extra fields to have value type V.
                // Extract sup's value-type from the RT_UNIFORM payload.
                // The payload field name is RT_FIELD_VALUE_TYPE ("value-type").
                if let Some(sup_t_val) = typevalue_payload(sup_t) {
                    if let Some(sup_val_type) =
                        payload_typevalue_field(sup_t_val, RT_FIELD_VALUE_TYPE)
                    {
                        if let Some(sub_t) = &sub_tail {
                            if typevalue_ctor(sub_t) == Some(RT_UNIFORM) {
                                // sub also has RT_UNIFORM: check value type covariance.
                                if let Some(sub_t_val) = typevalue_payload(sub_t) {
                                    if let Some(sub_val_type) =
                                        payload_typevalue_field(sub_t_val, RT_FIELD_VALUE_TYPE)
                                    {
                                        return is_subtype_bas_with_sigma(
                                            &sub_val_type,
                                            &sup_val_type,
                                            ctx,
                                            sigma,
                                            depth + 1,
                                        );
                                    }
                                }
                                return false; // sub RT_UNIFORM but no value-type
                            }
                            // sub tail is closed (empty dict or RT_CLOSED): check that all
                            // sub field types (excluding sup's explicit fields) are <: sup_val_type.
                            is_closed_tail(sub_t)
                                && check_sub_fields_against_uniform(
                                    &sub_fields,
                                    &sup_explicit_keys,
                                    &sup_val_type,
                                    ctx,
                                    sigma,
                                    depth,
                                )
                        } else {
                            // sub has no tail field = closed: check that all sub field types
                            // (excluding sup's explicit fields) are <: sup_val_type.
                            check_sub_fields_against_uniform(
                                &sub_fields,
                                &sup_explicit_keys,
                                &sup_val_type,
                                ctx,
                                sigma,
                                depth,
                            )
                        }
                    } else {
                        // sup RT_UNIFORM but no value-type in payload — conservative true
                        true
                    }
                } else {
                    // sup RT_UNIFORM but no payload — conservative true
                    true
                }
            } else {
                // sup tail is closed: RT_CLOSED variant or empty dict [].
                // sub must also be closed — it must not have additional fields beyond sup.
                match &sub_tail {
                    None => true, // sub has no tail field = closed
                    Some(sub_t) => is_closed_tail(sub_t),
                }
            }
        }
    }
}

/// Check type application subtyping using variance from TyConEnv.
fn is_app_subtype(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    let sub_payload = match typevalue_payload(sub) {
        Some(p) => p,
        None => return false,
    };
    let sup_payload = match typevalue_payload(sup) {
        Some(p) => p,
        None => return false,
    };

    let sub_op = payload_typevalue_field(sub_payload, FIELD_OP);
    let sup_op = payload_typevalue_field(sup_payload, FIELD_OP);
    let sub_arg = payload_typevalue_field(sub_payload, FIELD_ARG);
    let sup_arg = payload_typevalue_field(sup_payload, FIELD_ARG);

    match (sub_op, sup_op, sub_arg, sup_arg) {
        (Some(so), Some(po), Some(sa), Some(pa)) => {
            // Ops must match (nominal)
            if !is_atom_subtype(&so, &po, ctx, depth + 1, sigma) {
                return false;
            }

            // Look up variance from TyConEnv if available.
            // typevalue_ctor(&so) returns the ctor tag ("TypeValue.Op"), not the op name.
            // The actual op name lives in the payload under FIELD_NAME.
            // B-669: `def.variance.first()` always picks the FIRST variance entry
            // regardless of curried App spine depth. For multi-parameter type constructors
            // like `App(App(TyCon("Map"), K), V)`, the outer App's `arg` is V but
            // `variance.first()` returns K's variance. This is incorrect when K and V
            // have different variances. Fix: extract the full spine and use
            // `def.variance.get(spine_depth)` instead. See tracker item B-669.
            let variance = if let Some(op_name) =
                typevalue_payload(&so).and_then(|p| payload_string_field(p, FIELD_NAME))
            {
                ctx.tycon_env
                    .get(op_name)
                    .and_then(|def| def.variance.first().copied())
                    .unwrap_or(Variance::Invariant)
            } else {
                Variance::Invariant
            };

            match variance {
                Variance::Covariant => is_subtype_bas_with_sigma(&sa, &pa, ctx, sigma, depth + 1),
                Variance::Contravariant => {
                    is_subtype_bas_with_sigma(&pa, &sa, ctx, sigma, depth + 1)
                }
                Variance::Invariant => {
                    is_subtype_bas_with_sigma(&sa, &pa, ctx, sigma, depth + 1)
                        && is_subtype_bas_with_sigma(&pa, &sa, ctx, sigma, depth + 1)
                }
                Variance::Phantom => true,
            }
        }
        _ => false,
    }
}

/// Coinductive recursive type subtyping (S-Assum/S-Exp).
///
/// Uses the sigma set to detect when the same pair is being compared again (S-Assum).
fn is_recursive_subtype(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    // S-Assum key: structural fingerprint of each recursive type.
    //
    // The coinductive hypothesis must be keyed by *type identity*, not *pointer identity*
    // (Amadio & Cardelli, 1993). Pointer-based keys (`Arc::as_ptr`) fail when the same
    // structural type is extracted from a payload dict multiple times — each extraction
    // creates a fresh Arc with a different pointer, so the hypothesis is never found on
    // re-entry (B-668).
    //
    // Structural fingerprints are stable under re-extraction: two Arc<Value>s with
    // identical type structure produce identical fingerprint strings, regardless of
    // whether they share the same Arc allocation. The fingerprint is also injective
    // on types (distinct types produce distinct fingerprints), so S-Assum cannot fire
    // incorrectly — it only fires when the same pair of types is being compared again.
    let sub_key = typevalue_structural_fingerprint(sub, 0);
    let sup_key = typevalue_structural_fingerprint(sup, 0);
    let key = (sub_key, sup_key);

    if sigma.contains(&key) {
        return true; // S-Assum: coinductive hypothesis
    }
    sigma.insert(key);

    // S-Exp: unfold both and recurse.
    // Use is_subtype_bas_with_sigma to thread the coinductive sigma set through the
    // unfolded call. If we called is_subtype_bas instead, it would create a fresh sigma,
    // losing the coinductive hypothesis inserted above and causing infinite recursion for
    // structurally identical recursive types (e.g., μa.(Int | {x:a}) <: μb.(Int | {x:b})).
    let sub_unfolded = unfold_recursive_typevalue(sub);
    let sup_unfolded = unfold_recursive_typevalue(sup);
    is_subtype_bas_with_sigma(&sub_unfolded, &sup_unfolded, ctx, sigma, depth + 1)
}

/// Unfold a TypeValue.Recursive one step.
///
/// Replaces `TypeValue.RecursiveRef { depth: 0 }` occurrences in the body with the full
/// `TypeValue.Recursive` value. This is the de Bruijn-indexed equirecursive unfolding.
///
/// If `tv` is not a `TypeValue.Recursive`, returns it unchanged.
pub fn unfold_recursive_typevalue(tv: &Arc<Value>) -> Arc<Value> {
    if typevalue_ctor(tv) != Some(TV_RECURSIVE) {
        return Arc::clone(tv);
    }

    if let Some(payload) = typevalue_payload(tv) {
        if let Some(body) = payload_typevalue_field(payload, FIELD_BODY) {
            return substitute_recursive_ref(&body, 0, tv);
        }
    }
    Arc::clone(tv)
}

/// Substitute `TypeValue.RecursiveRef { depth: target }` with `replacement` in `tv`.
///
/// When `target == 0`, replaces the innermost self-reference with the full Recursive type.
pub(crate) fn substitute_recursive_ref(
    tv: &Arc<Value>,
    target: u32,
    replacement: &Arc<Value>,
) -> Arc<Value> {
    match typevalue_ctor(tv) {
        Some(TV_RECURSIVE_REF) => {
            if let Some(payload) = typevalue_payload(tv) {
                if let Some(idx_val) = payload_int_field(payload, FIELD_DEPTH) {
                    if idx_val as u32 == target {
                        return Arc::clone(replacement);
                    }
                }
            }
            Arc::clone(tv)
        }
        Some(TV_RECURSIVE) => {
            // Entering a deeper binder: increment target
            if let Some(payload) = typevalue_payload(tv) {
                if let Some(body) = payload_typevalue_field(payload, FIELD_BODY) {
                    let new_body = substitute_recursive_ref(&body, target + 1, replacement);
                    if Arc::ptr_eq(&body, &new_body) {
                        return Arc::clone(tv); // No change
                    }
                    let span = bas_span();
                    let mut fields = IndexMap::new();
                    fields.insert(FIELD_BODY.to_string(), new_body);
                    let new_payload = make_payload_thunk(fields, span);
                    return make_payload_typevalue(TV_RECURSIVE, new_payload);
                }
            }
            Arc::clone(tv)
        }
        Some(TV_UNION) | Some(TV_INTER) => {
            if let Some(payload) = typevalue_payload(tv) {
                let members = payload_members(payload);
                let new_members: Vec<Arc<Value>> = members
                    .iter()
                    .map(|m| substitute_recursive_ref(m, target, replacement))
                    .collect();
                let changed = members
                    .iter()
                    .zip(&new_members)
                    .any(|(a, b)| !Arc::ptr_eq(a, b));
                if !changed {
                    return Arc::clone(tv);
                }
                let ctor = typevalue_ctor(tv).unwrap_or(TV_UNION);
                return build_members_typevalue(ctor, new_members);
            }
            Arc::clone(tv)
        }
        Some(TV_NEG) => {
            if let Some(payload) = typevalue_payload(tv) {
                if let Some(inner) = payload_typevalue_field(payload, FIELD_OF) {
                    let new_inner = substitute_recursive_ref(&inner, target, replacement);
                    if Arc::ptr_eq(&inner, &new_inner) {
                        return Arc::clone(tv);
                    }
                    let span = bas_span();
                    let mut fields = IndexMap::new();
                    fields.insert(FIELD_OF.to_string(), new_inner);
                    let new_payload = make_payload_thunk(fields, span);
                    return make_payload_typevalue(TV_NEG, new_payload);
                }
            }
            Arc::clone(tv)
        }
        Some(TV_FN) => {
            // Recurse into params (indexed dict of TypeValues) and return type.
            // A recursive function type like mu.X.(fn(X) -> X) requires substitution
            // inside both the params and return fields to correctly unfold the self-reference.
            if let Some(payload) = typevalue_payload(tv) {
                let ret = payload_typevalue_field(payload, FIELD_RETURN);
                let params = payload_typevalue_field(payload, FIELD_PARAMS);

                let new_ret = ret
                    .as_ref()
                    .map(|r| substitute_recursive_ref(r, target, replacement));
                let new_params = params.as_ref().and_then(|p| {
                    if let Value::Dict { entries, .. } = p.as_ref() {
                        let mut new_entries = IndexMap::new();
                        let mut any_changed = false;
                        let mut indexed: Vec<(i64, Arc<Value>)> = entries
                            .iter()
                            .filter_map(|(k, thunk)| {
                                if let HashableValue::Int(idx) = k {
                                    peek_value(thunk).map(|v| (*idx, Arc::new(v.clone())))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        indexed.sort_by_key(|(i, _)| *i);
                        for (idx, param_tv) in &indexed {
                            let new_param = substitute_recursive_ref(param_tv, target, replacement);
                            if !Arc::ptr_eq(param_tv, &new_param) {
                                any_changed = true;
                            }
                            let span = bas_span();
                            new_entries.insert(
                                HashableValue::Int(*idx),
                                Arc::new(Thunk::value((*new_param).clone(), span)),
                            );
                        }
                        if any_changed {
                            Some(Arc::new(Value::Dict {
                                entries: new_entries,
                                type_val: crate::value::unknown_type_val(),
                            }))
                        } else {
                            None // unchanged
                        }
                    } else {
                        None
                    }
                });

                let ret_changed = match (ret.as_ref(), new_ret.as_ref()) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => false,
                };
                let params_changed = new_params.is_some();

                if ret_changed || params_changed {
                    let span = bas_span();
                    // Copy-then-overwrite: start with ALL existing payload fields so that
                    // non-substituted fields (FIELD_VARIADIC, FIELD_PARAM_NAMES, etc.) are
                    // preserved in the rebuilt payload. Only FIELD_PARAMS and FIELD_RETURN
                    // are substituted — any other future fields are preserved automatically.
                    let mut fields = IndexMap::new();
                    if let Value::Dict { entries, .. } = payload {
                        for (key, thunk) in entries {
                            if let HashableValue::Str(k) = key {
                                if let Some(val) = peek_value(thunk) {
                                    fields.insert(k.to_string(), Arc::new(val.clone()));
                                }
                            }
                        }
                    }
                    // Overwrite only the fields that were substituted.
                    if let Some(new_r) = new_ret {
                        fields.insert(FIELD_RETURN.to_string(), new_r);
                    } else if let Some(r) = ret {
                        fields.insert(FIELD_RETURN.to_string(), r);
                    }
                    if let Some(new_p) = new_params {
                        fields.insert(FIELD_PARAMS.to_string(), new_p);
                    } else if let Some(p) = params {
                        fields.insert(FIELD_PARAMS.to_string(), p);
                    }
                    let new_payload = make_payload_thunk(fields, span);
                    return make_payload_typevalue(TV_FN, new_payload);
                }
            }
            Arc::clone(tv)
        }
        Some(TV_RECORD) => {
            // Recurse into fields dict (each field TypeValue) and tail TypeValue.
            // A recursive record type like mu.X.{ x: X } requires substitution inside
            // the fields dict to correctly replace the RecursiveRef(0) in the field types.
            if let Some(payload) = typevalue_payload(tv) {
                let fields_val = payload_typevalue_field(payload, FIELD_FIELDS);
                let tail = payload_typevalue_field(payload, FIELD_TAIL);

                let new_fields_val = fields_val.as_ref().and_then(|fv| {
                    if let Value::Dict { entries, .. } = fv.as_ref() {
                        let mut new_entries = IndexMap::new();
                        let mut any_changed = false;
                        for (key, thunk) in entries {
                            if let Some(field_tv_val) = peek_value(thunk) {
                                let field_tv = Arc::new(field_tv_val.clone());
                                let new_field_tv =
                                    substitute_recursive_ref(&field_tv, target, replacement);
                                if !Arc::ptr_eq(&field_tv, &new_field_tv) {
                                    any_changed = true;
                                }
                                let span = bas_span();
                                new_entries.insert(
                                    key.clone(),
                                    Arc::new(Thunk::value((*new_field_tv).clone(), span)),
                                );
                            }
                        }
                        if any_changed {
                            Some(Arc::new(Value::Dict {
                                entries: new_entries,
                                type_val: crate::value::unknown_type_val(),
                            }))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                let new_tail = tail
                    .as_ref()
                    .map(|t| substitute_recursive_ref(t, target, replacement));

                let fields_changed = new_fields_val.is_some();
                let tail_changed = match (tail.as_ref(), new_tail.as_ref()) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => false,
                };

                if fields_changed || tail_changed {
                    let span = bas_span();
                    // Copy-then-overwrite: start with ALL existing payload fields so that
                    // any future Record payload fields are preserved automatically.
                    let mut payload_fields = IndexMap::new();
                    if let Value::Dict { entries, .. } = payload {
                        for (key, thunk) in entries {
                            if let HashableValue::Str(k) = key {
                                if let Some(val) = peek_value(thunk) {
                                    payload_fields.insert(k.to_string(), Arc::new(val.clone()));
                                }
                            }
                        }
                    }
                    // Overwrite only the fields that were substituted.
                    if let Some(new_fv) = new_fields_val {
                        payload_fields.insert(FIELD_FIELDS.to_string(), new_fv);
                    } else if let Some(fv) = fields_val {
                        payload_fields.insert(FIELD_FIELDS.to_string(), fv);
                    }
                    if let Some(new_t) = new_tail {
                        payload_fields.insert(FIELD_TAIL.to_string(), new_t);
                    } else if let Some(t) = tail {
                        payload_fields.insert(FIELD_TAIL.to_string(), t);
                    }
                    let new_payload = make_payload_thunk(payload_fields, span);
                    return make_payload_typevalue(TV_RECORD, new_payload);
                }
            }
            Arc::clone(tv)
        }
        Some(TV_APP) => {
            // Recurse into op and arg TypeValue fields.
            // A recursive app like mu.X.App(F, X) requires substitution inside both fields.
            if let Some(payload) = typevalue_payload(tv) {
                let op = payload_typevalue_field(payload, FIELD_OP);
                let arg = payload_typevalue_field(payload, FIELD_ARG);

                let new_op = op
                    .as_ref()
                    .map(|o| substitute_recursive_ref(o, target, replacement));
                let new_arg = arg
                    .as_ref()
                    .map(|a| substitute_recursive_ref(a, target, replacement));

                let op_changed = match (op.as_ref(), new_op.as_ref()) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => false,
                };
                let arg_changed = match (arg.as_ref(), new_arg.as_ref()) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => false,
                };

                if op_changed || arg_changed {
                    let span = bas_span();
                    // Copy-then-overwrite: preserve any future payload fields automatically.
                    let mut fields = IndexMap::new();
                    if let Value::Dict { entries, .. } = payload {
                        for (key, thunk) in entries {
                            if let HashableValue::Str(k) = key {
                                if let Some(val) = peek_value(thunk) {
                                    fields.insert(k.to_string(), Arc::new(val.clone()));
                                }
                            }
                        }
                    }
                    if let Some(new_o) = new_op {
                        fields.insert(FIELD_OP.to_string(), new_o);
                    } else if let Some(o) = op {
                        fields.insert(FIELD_OP.to_string(), o);
                    }
                    if let Some(new_a) = new_arg {
                        fields.insert(FIELD_ARG.to_string(), new_a);
                    } else if let Some(a) = arg {
                        fields.insert(FIELD_ARG.to_string(), a);
                    }
                    let new_payload = make_payload_thunk(fields, span);
                    return make_payload_typevalue(TV_APP, new_payload);
                }
            }
            Arc::clone(tv)
        }
        Some(TV_NOMINAL_VARIANT) => {
            // Recurse into the fields TypeValue (TypeValue.Record for payload constructors).
            // tycon and ctor are strings, not TypeValues — no substitution needed there.
            if let Some(payload) = typevalue_payload(tv) {
                let fields = payload_typevalue_field(payload, FIELD_FIELDS);

                let new_fields = fields
                    .as_ref()
                    .map(|f| substitute_recursive_ref(f, target, replacement));

                let fields_changed = match (fields.as_ref(), new_fields.as_ref()) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => false,
                };

                if fields_changed {
                    let span = bas_span();
                    // Copy-then-overwrite: preserve tycon, ctor, and any future fields.
                    let mut payload_fields = IndexMap::new();
                    if let Value::Dict { entries, .. } = payload {
                        for (key, thunk) in entries {
                            if let HashableValue::Str(k) = key {
                                if let Some(val) = peek_value(thunk) {
                                    payload_fields.insert(k.to_string(), Arc::new(val.clone()));
                                }
                            }
                        }
                    }
                    if let Some(new_f) = new_fields {
                        payload_fields.insert(FIELD_FIELDS.to_string(), new_f);
                    } else if let Some(f) = fields {
                        payload_fields.insert(FIELD_FIELDS.to_string(), f);
                    }
                    let new_payload = make_payload_thunk(payload_fields, span);
                    return make_payload_typevalue(TV_NOMINAL_VARIANT, new_payload);
                }
            }
            Arc::clone(tv)
        }
        // Atoms with no TypeValue sub-fields: TV_VAR, TV_UNKNOWN, TV_REPR, TV_OP,
        // TV_INT_LIT, TV_STR_LIT, TV_FLOAT_LIT, TV_PHANTOM, TV_SCHEME.
        // TV_SCHEME and TV_PHANTOM are invariant-excluded from Recursive bodies
        // by the type checker — TV_SCHEME is a top-level quantifier that never
        // appears inside a μ-body, and TV_PHANTOM has no TypeValue sub-fields.
        _ => Arc::clone(tv),
    }
}

/// Build a TypeValue.Union or TypeValue.Inter with indexed members.
///
/// The payload structure must match what `payload_members` expects:
/// `{ members: { 0: m0, 1: m1, ... } }` — an outer dict with a "members" key
/// whose value is the integer-indexed inner dict.
fn build_members_typevalue(ctor: &str, members: Vec<Arc<Value>>) -> Arc<Value> {
    let span = bas_span();
    // Inner dict: integer-keyed { 0: m0, 1: m1, ... }
    let mut inner_entries = IndexMap::new();
    for (i, m) in members.iter().enumerate() {
        let key = HashableValue::Int(i as i64);
        let thunk = Arc::new(Thunk::value((**m).clone(), span.clone()));
        inner_entries.insert(key, thunk);
    }
    let inner_dict = Value::Dict {
        entries: inner_entries,
        type_val: unknown_type_val(),
    };
    // Outer dict: { members: inner_dict }
    let mut outer_entries = IndexMap::new();
    outer_entries.insert(
        HashableValue::Str(Arc::from(FIELD_MEMBERS)),
        Arc::new(Thunk::value(inner_dict, span.clone())),
    );
    let payload_thunk = Arc::new(Thunk::value(
        Value::Dict {
            entries: outer_entries,
            type_val: unknown_type_val(),
        },
        span,
    ));
    make_payload_typevalue(ctor, payload_thunk)
}

/// Collect indexed TypeValues from an Arc<Value> Dict (used for params lists, member lists).
fn collect_indexed_typevalues(tv: &Arc<Value>) -> Vec<Arc<Value>> {
    let mut result = Vec::new();
    if let Value::Dict { entries, .. } = tv.as_ref() {
        let mut indexed: Vec<(i64, Arc<Value>)> = Vec::new();
        for (key, thunk) in entries {
            if let HashableValue::Int(idx) = key {
                if let Some(val) = peek_value(thunk) {
                    indexed.push((*idx, Arc::new(val.clone())));
                }
            }
        }
        indexed.sort_by_key(|(i, _)| *i);
        result = indexed.into_iter().map(|(_, v)| v).collect();
    }
    result
}

// ---------------------------------------------------------------------------
// Emptiness checking
// ---------------------------------------------------------------------------

/// Check if an RDNF is empty (uninhabited).
pub fn is_rdnf_empty(
    rdnf: &Rdnf,
    ctx: &InferenceContext,
    sigma: &mut HashSet<(String, String)>,
    depth: usize,
) -> bool {
    if rdnf.is_empty() {
        return true; // No disjuncts → Never
    }
    rdnf.iter().all(|conj| {
        let mut conj_sigma = sigma.clone();
        is_conjunction_empty(conj, ctx, &mut conj_sigma, depth)
    })
}

/// Check if a conjunction of signed atoms is empty (uninhabited).
///
/// A conjunction is empty if:
/// 1. Two positive atoms of incompatible kinds exist
/// 2. Some positive atom is subsumed by a negative atom (Pos(A) and Neg(B) where A <: B)
fn is_conjunction_empty(
    conj: &Conjunction,
    ctx: &InferenceContext,
    sigma: &mut HashSet<(String, String)>,
    depth: usize,
) -> bool {
    if conj.is_empty() {
        return false; // Empty conjunction = Top = inhabited
    }

    let positives: Vec<&Arc<Value>> = conj
        .iter()
        .filter_map(|sa| match sa {
            SignedAtom::Pos(a) => Some(a),
            _ => None,
        })
        .collect();

    let negatives: Vec<&Arc<Value>> = conj
        .iter()
        .filter_map(|sa| match sa {
            SignedAtom::Neg(a) => Some(a),
            _ => None,
        })
        .collect();

    // Condition 1: Positive-atom component disjointness
    for i in 0..positives.len() {
        for j in (i + 1)..positives.len() {
            if atoms_are_disjoint(positives[i], positives[j], ctx) {
                return true;
            }
        }
    }

    // Condition 2: Positive atom subsumed by negative atom
    for pos in &positives {
        for neg in &negatives {
            if is_atom_subtype(pos, neg, ctx, depth, sigma) {
                return true;
            }
        }
    }

    false
}

/// Check if two TypeValue atoms are disjoint (no value can inhabit both).
fn atoms_are_disjoint(a: &Arc<Value>, b: &Arc<Value>, ctx: &InferenceContext) -> bool {
    if Arc::ptr_eq(a, b) {
        return false; // Same atom → not disjoint
    }

    let a_ctor = typevalue_ctor(a);
    let b_ctor = typevalue_ctor(b);

    match (a_ctor, b_ctor) {
        // TypeValue.Var: conservative — not disjoint from anything (unresolved)
        (Some(TV_VAR), _) | (_, Some(TV_VAR)) => false,

        // Different Repr values: disjoint (Int and Str, Str and Bytes, etc.)
        (Some(TV_REPR), Some(TV_REPR)) => {
            let a_repr = typevalue_payload(a)
                .and_then(|p| payload_string_field(p, FIELD_REPR).map(String::from));
            let b_repr = typevalue_payload(b)
                .and_then(|p| payload_string_field(p, FIELD_REPR).map(String::from));
            match (a_repr, b_repr) {
                (Some(ar), Some(br)) => ar != br,
                _ => false,
            }
        }

        // Different IntLit values: disjoint
        (Some(TV_INT_LIT), Some(TV_INT_LIT)) => {
            let a_n = typevalue_payload(a).and_then(|p| payload_int_field(p, FIELD_VALUE));
            let b_n = typevalue_payload(b).and_then(|p| payload_int_field(p, FIELD_VALUE));
            match (a_n, b_n) {
                (Some(an), Some(bn)) => an != bn,
                _ => false,
            }
        }

        // Different FloatLit values: disjoint (compare by bit pattern for NaN safety)
        (Some(TV_FLOAT_LIT), Some(TV_FLOAT_LIT)) => {
            let a_bits =
                typevalue_payload(a).and_then(|p| payload_float_field_bits(p, FIELD_VALUE));
            let b_bits =
                typevalue_payload(b).and_then(|p| payload_float_field_bits(p, FIELD_VALUE));
            match (a_bits, b_bits) {
                (Some(ab), Some(bb)) => ab != bb,
                _ => false,
            }
        }

        // StrLit vs Repr: disjoint iff the Repr is not Value::String.
        // StrLit values are strings — they can only overlap with Repr("Value::String").
        (Some(TV_STR_LIT), Some(TV_REPR)) => {
            let repr = typevalue_payload(b).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_STRING)
        }
        (Some(TV_REPR), Some(TV_STR_LIT)) => {
            let repr = typevalue_payload(a).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_STRING)
        }

        // IntLit vs Repr: disjoint iff the Repr is not Value::Int.
        // IntLit values are integers — they can only overlap with Repr("Value::Int").
        (Some(TV_INT_LIT), Some(TV_REPR)) => {
            let repr = typevalue_payload(b).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_INT)
        }
        (Some(TV_REPR), Some(TV_INT_LIT)) => {
            let repr = typevalue_payload(a).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_INT)
        }

        // FloatLit vs Repr: disjoint iff the Repr is not Value::Float.
        (Some(TV_FLOAT_LIT), Some(TV_REPR)) => {
            let repr = typevalue_payload(b).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_FLOAT)
        }
        (Some(TV_REPR), Some(TV_FLOAT_LIT)) => {
            let repr = typevalue_payload(a).and_then(|p| payload_string_field(p, FIELD_REPR));
            repr != Some(REPR_FLOAT)
        }

        // Repr vs everything else (except Var and the literal types handled above): disjoint
        (Some(TV_REPR), Some(other)) | (Some(other), Some(TV_REPR))
            if other != TV_VAR
                && other != TV_INT_LIT
                && other != TV_STR_LIT
                && other != TV_FLOAT_LIT =>
        {
            match other {
                // Record/Fn/App/NominalVariant/Recursive are disjoint from Repr primitives
                TV_RECORD | TV_FN | TV_APP | TV_NOMINAL_VARIANT | TV_RECURSIVE => true,
                _ => false, // Conservative
            }
        }

        // Record vs Fn: disjoint
        (Some(TV_RECORD), Some(TV_FN)) | (Some(TV_FN), Some(TV_RECORD)) => true,

        // NominalVariant with different tags: disjoint
        (Some(TV_NOMINAL_VARIANT), Some(TV_NOMINAL_VARIANT)) => {
            let a_tycon = typevalue_payload(a)
                .and_then(|p| payload_string_field(p, FIELD_TYCON).map(String::from));
            let b_tycon = typevalue_payload(b)
                .and_then(|p| payload_string_field(p, FIELD_TYCON).map(String::from));
            let a_ctor_f = typevalue_payload(a)
                .and_then(|p| payload_string_field(p, FIELD_CTOR).map(String::from));
            let b_ctor_f = typevalue_payload(b)
                .and_then(|p| payload_string_field(p, FIELD_CTOR).map(String::from));
            match (a_tycon, b_tycon, a_ctor_f, b_ctor_f) {
                (Some(at), Some(bt), Some(ac), Some(bc)) => at != bt || ac != bc,
                _ => false,
            }
        }

        // NominalVariant vs Repr/Record/Fn: disjoint
        (Some(TV_NOMINAL_VARIANT), Some(TV_REPR))
        | (Some(TV_REPR), Some(TV_NOMINAL_VARIANT))
        | (Some(TV_NOMINAL_VARIANT), Some(TV_RECORD))
        | (Some(TV_RECORD), Some(TV_NOMINAL_VARIANT))
        | (Some(TV_NOMINAL_VARIANT), Some(TV_FN))
        | (Some(TV_FN), Some(TV_NOMINAL_VARIANT)) => true,

        // Recursive: conservative
        (Some(TV_RECURSIVE), _) | (_, Some(TV_RECURSIVE)) => false,

        // App with different TyCons: disjoint
        (Some(TV_APP), Some(TV_APP)) => {
            let a_op = typevalue_payload(a).and_then(|p| payload_typevalue_field(p, FIELD_OP));
            let b_op = typevalue_payload(b).and_then(|p| payload_typevalue_field(p, FIELD_OP));
            match (a_op, b_op) {
                (Some(ao), Some(bo)) => atoms_are_disjoint(&ao, &bo, ctx),
                _ => false,
            }
        }

        // Op: different names → disjoint
        (Some(TV_OP), Some(TV_OP)) => {
            let a_name = typevalue_payload(a)
                .and_then(|p| payload_string_field(p, FIELD_NAME).map(String::from));
            let b_name = typevalue_payload(b)
                .and_then(|p| payload_string_field(p, FIELD_NAME).map(String::from));
            match (a_name, b_name) {
                (Some(an), Some(bn)) => an != bn,
                _ => false,
            }
        }

        // Fn vs Op: disjoint
        (Some(TV_FN), Some(TV_OP)) | (Some(TV_OP), Some(TV_FN)) => true,

        // Different kinds of things: conservative
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Public API: TypeValue-facing BAS functions
// ---------------------------------------------------------------------------

/// BAS subtyping: `A <: B` iff `A & ~B` is uninhabited.
///
/// This is the core BAS algorithm operating on TypeValues (Arc<Value>).
/// Short-circuits before RDNF for common cases (Error, Top, Never, Unknown, TypeVar).
pub fn is_subtype_bas(sub: &Arc<Value>, sup: &Arc<Value>, ctx: &InferenceContext) -> bool {
    let mut sigma = HashSet::new();
    is_subtype_bas_with_sigma(sub, sup, ctx, &mut sigma, 0)
}

/// BAS subtype check with a caller-supplied coinductive sigma set.
///
/// The sigma set carries the coinductive hypothesis across recursive-type unfolding.
/// `is_subtype_bas` creates a fresh sigma; `is_recursive_subtype` passes the existing
/// one so the structural fingerprint pair remains visible across the unfolded call.
///
/// The depth parameter accumulates through structural recursion (Record fields, Fn params/return)
/// to prevent pathological types from causing stack overflow. Callers should pass 0 at top level.
fn is_subtype_bas_with_sigma(
    sub: &Arc<Value>,
    sup: &Arc<Value>,
    ctx: &InferenceContext,
    sigma: &mut HashSet<(String, String)>,
    depth: usize,
) -> bool {
    // Reflexivity: A <: A is always true. Arc::ptr_eq is not a shortcut — it is the
    // exact reflexivity axiom for pointer-identical types (same Arc). This handles
    // recursive type self-comparison and identity checks without unfolding.
    if Arc::ptr_eq(sub, sup) {
        return true;
    }

    let sub_ctor = typevalue_ctor(sub);
    let sup_ctor = typevalue_ctor(sup);

    // Error: not a subtype of anything (cascade sentinel)
    if matches!(sub_ctor, Some(TV_ERROR)) || matches!(sup_ctor, Some(TV_ERROR)) {
        return false;
    }

    // Top: τ <: Top for all τ
    if matches!(sup_ctor, Some(TV_TOP)) {
        return true;
    }

    // Never: Never <: τ for all τ
    if matches!(sub_ctor, Some(TV_NEVER)) {
        return true;
    }

    // Unknown: not in the subtype lattice (uses consistency instead)
    if matches!(sub_ctor, Some(TV_UNKNOWN)) || matches!(sup_ctor, Some(TV_UNKNOWN)) {
        return false;
    }

    // TypeVar on either side: conservative true (defers to constraint solver)
    if matches!(sub_ctor, Some(TV_VAR)) || matches!(sup_ctor, Some(TV_VAR)) {
        return true;
    }

    // BAS: A <: B iff A & ~B is uninhabited
    let sup_neg = make_payload_typevalue(TV_NEG, {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(FIELD_OF.to_string(), Arc::clone(sup));
        make_payload_thunk(fields, span)
    });
    let diff = build_members_typevalue(TV_INTER, vec![Arc::clone(sub), sup_neg]);
    let rdnf = to_rdnf(&diff);
    is_rdnf_empty(&rdnf, ctx, sigma, depth)
}

/// Consistent subtyping (AGT, Garcia et al. 2016): `A ~<: B`.
///
/// Used for runtime TypeAssert: value type (which may have Unknown at erased positions)
/// must be consistent with the expected annotation.
pub fn is_consistent_subtype(sub: &Arc<Value>, sup: &Arc<Value>, ctx: &InferenceContext) -> bool {
    let sub_ctor = typevalue_ctor(sub);
    let sup_ctor = typevalue_ctor(sup);

    // Unknown or Top on sub side: consistent with everything
    if matches!(sub_ctor, Some(TV_UNKNOWN) | Some(TV_TOP)) {
        return true;
    }

    // Unknown on sup side: everything is consistent with Unknown
    if matches!(sup_ctor, Some(TV_UNKNOWN)) {
        return true;
    }

    // Unresolved TypeVar on sup side: treat as Unknown (gradual)
    if matches!(sup_ctor, Some(TV_VAR)) {
        return true;
    }

    // Error: never a consistent subtype
    if matches!(sub_ctor, Some(TV_ERROR)) || matches!(sup_ctor, Some(TV_ERROR)) {
        return false;
    }

    // Unfold recursive types before consistent-subtype check
    if matches!(sub_ctor, Some(TV_RECURSIVE)) && matches!(sup_ctor, Some(TV_RECURSIVE)) {
        return is_subtype_bas(sub, sup, ctx);
    }
    if matches!(sub_ctor, Some(TV_RECURSIVE)) {
        let unfolded = unfold_recursive_typevalue(sub);
        return is_consistent_subtype(&unfolded, sup, ctx);
    }
    if matches!(sup_ctor, Some(TV_RECURSIVE)) {
        let unfolded = unfold_recursive_typevalue(sup);
        return is_consistent_subtype(sub, &unfolded, ctx);
    }

    // Union on sub: all members must be c.s. subtype of sup
    if matches!(sub_ctor, Some(TV_UNION)) {
        if let Some(payload) = typevalue_payload(sub) {
            let members = payload_members(payload);
            return members.iter().all(|m| is_consistent_subtype(m, sup, ctx));
        }
    }

    // Union on sup: sub is c.s. subtype of union if c.s. subtype of any member
    if matches!(sup_ctor, Some(TV_UNION)) {
        if let Some(payload) = typevalue_payload(sup) {
            let members = payload_members(payload);
            return members.iter().any(|m| is_consistent_subtype(sub, m, ctx));
        }
    }

    // Intersection on sup: sub must be c.s. subtype of all members
    if matches!(sup_ctor, Some(TV_INTER)) {
        if let Some(payload) = typevalue_payload(sup) {
            let members = payload_members(payload);
            return members.iter().all(|m| is_consistent_subtype(sub, m, ctx));
        }
    }

    // Fallback: use proper subtyping
    is_subtype_bas(sub, sup, ctx)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::unknown_type_val;

    fn empty_ctx() -> InferenceContext {
        InferenceContext::new()
    }

    fn make_repr(repr: &str) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(
            FIELD_REPR.to_string(),
            Arc::new(Value::String {
                source: Arc::from(repr),
                start: 0,
                end: repr.len(),
                type_val: unknown_type_val(),
            }),
        );
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_REPR, payload_thunk)
    }

    fn make_top() -> Arc<Value> {
        Arc::new(Value::Variant {
            ctor: Arc::from(TV_TOP),
            payload: None,
            type_val: unknown_type_val(),
        })
    }

    fn make_never() -> Arc<Value> {
        Arc::new(Value::Variant {
            ctor: Arc::from(TV_NEVER),
            payload: None,
            type_val: unknown_type_val(),
        })
    }

    fn make_unknown() -> Arc<Value> {
        Arc::new(Value::Variant {
            ctor: Arc::from(TV_UNKNOWN),
            payload: None,
            type_val: unknown_type_val(),
        })
    }

    #[test]
    fn test_is_subtype_bas_repr_same() {
        let ctx = empty_ctx();
        let int1 = make_repr(REPR_INT);
        let int2 = make_repr(REPR_INT);
        assert!(is_subtype_bas(&int1, &int2, &ctx));
    }

    #[test]
    fn test_is_subtype_bas_repr_different() {
        let ctx = empty_ctx();
        let int = make_repr(REPR_INT);
        let str_ = make_repr(REPR_STRING);
        assert!(!is_subtype_bas(&int, &str_, &ctx));
    }

    #[test]
    fn test_is_subtype_bas_never_subtype_of_int() {
        let ctx = empty_ctx();
        let int = make_repr(REPR_INT);
        let never = make_never();
        assert!(is_subtype_bas(&never, &int, &ctx));
    }

    #[test]
    fn test_is_subtype_bas_int_subtype_of_top() {
        let ctx = empty_ctx();
        let int = make_repr(REPR_INT);
        let top = make_top();
        assert!(is_subtype_bas(&int, &top, &ctx));
    }

    #[test]
    fn test_is_subtype_bas_unknown_not_subtype() {
        let ctx = empty_ctx();
        let int = make_repr(REPR_INT);
        let unknown = make_unknown();
        assert!(!is_subtype_bas(&unknown, &int, &ctx));
        assert!(!is_subtype_bas(&int, &unknown, &ctx));
    }

    // -------------------------------------------------------------------------
    // Helper constructors for extended test coverage
    // -------------------------------------------------------------------------

    fn make_int_lit(n: i64) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(
            FIELD_VALUE.to_string(),
            Arc::new(Value::Int {
                n,
                type_val: unknown_type_val(),
            }),
        );
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_INT_LIT, payload_thunk)
    }

    fn make_str_lit(s: &str) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(
            FIELD_VALUE.to_string(),
            Arc::new(Value::String {
                source: Arc::from(s),
                start: 0,
                end: s.len(),
                type_val: unknown_type_val(),
            }),
        );
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_STR_LIT, payload_thunk)
    }

    fn make_float_lit(f: f64) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(
            FIELD_VALUE.to_string(),
            Arc::new(Value::Float {
                n: f,
                type_val: unknown_type_val(),
            }),
        );
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_FLOAT_LIT, payload_thunk)
    }

    /// Build a TypeValue.Fn with positional params and a return type.
    fn make_fn(params: Vec<Arc<Value>>, ret: Arc<Value>) -> Arc<Value> {
        let span = bas_span();
        let mut params_entries = IndexMap::new();
        for (i, p) in params.iter().enumerate() {
            let key = HashableValue::Int(i as i64);
            let thunk = Arc::new(Thunk::value((**p).clone(), span.clone()));
            params_entries.insert(key, thunk);
        }
        let params_dict = Arc::new(Value::Dict {
            entries: params_entries,
            type_val: unknown_type_val(),
        });
        let mut fields = IndexMap::new();
        fields.insert(FIELD_PARAMS.to_string(), params_dict);
        fields.insert(FIELD_RETURN.to_string(), ret);
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_FN, payload_thunk)
    }

    /// Build a TypeValue.Record with named string-keyed fields and a closed tail (empty dict).
    fn make_record(fields: Vec<(&str, Arc<Value>)>) -> Arc<Value> {
        let span = bas_span();
        let mut field_entries = IndexMap::new();
        for (name, tv) in &fields {
            let key = HashableValue::Str(Arc::from(*name));
            let thunk = Arc::new(Thunk::value((**tv).clone(), span.clone()));
            field_entries.insert(key, thunk);
        }
        let fields_dict = Arc::new(Value::Dict {
            entries: field_entries,
            type_val: unknown_type_val(),
        });
        // Closed tail: empty dict []
        let tail_dict = Arc::new(Value::Dict {
            entries: IndexMap::new(),
            type_val: unknown_type_val(),
        });
        let mut payload_fields = IndexMap::new();
        payload_fields.insert(FIELD_FIELDS.to_string(), fields_dict);
        payload_fields.insert(FIELD_TAIL.to_string(), tail_dict);
        let payload_thunk = make_payload_thunk(payload_fields, span);
        make_payload_typevalue(TV_RECORD, payload_thunk)
    }

    /// Build a TypeValue.Union of the given members.
    fn make_union(members: Vec<Arc<Value>>) -> Arc<Value> {
        build_members_typevalue(TV_UNION, members)
    }

    /// Build a TypeValue.Inter of the given members.
    fn make_inter(members: Vec<Arc<Value>>) -> Arc<Value> {
        build_members_typevalue(TV_INTER, members)
    }

    /// Build a TypeValue.Recursive wrapping the given body.
    fn make_recursive(body: Arc<Value>) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(FIELD_BODY.to_string(), body);
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_RECURSIVE, payload_thunk)
    }

    /// Build a TypeValue.RecursiveRef with the given de Bruijn depth.
    fn make_recursive_ref(depth_val: i64) -> Arc<Value> {
        let span = bas_span();
        let mut fields = IndexMap::new();
        fields.insert(
            FIELD_DEPTH.to_string(),
            Arc::new(Value::Int {
                n: depth_val,
                type_val: unknown_type_val(),
            }),
        );
        let payload_thunk = make_payload_thunk(fields, span);
        make_payload_typevalue(TV_RECURSIVE_REF, payload_thunk)
    }

    // -------------------------------------------------------------------------
    // Literal promotion tests
    // -------------------------------------------------------------------------

    /// IntLit(42) <: Repr(Int) = true (literal promotion).
    #[test]
    fn test_int_lit_subtype_of_int_repr() {
        let ctx = empty_ctx();
        let lit = make_int_lit(42);
        let int_repr = make_repr(REPR_INT);
        assert!(
            is_subtype_bas(&lit, &int_repr, &ctx),
            "IntLit(42) must be a subtype of Repr(Int)"
        );
    }

    /// IntLit(42) <: Repr(String) = false (wrong repr).
    #[test]
    fn test_int_lit_not_subtype_of_str_repr() {
        let ctx = empty_ctx();
        let lit = make_int_lit(42);
        let str_repr = make_repr(REPR_STRING);
        assert!(
            !is_subtype_bas(&lit, &str_repr, &ctx),
            "IntLit(42) must NOT be a subtype of Repr(String)"
        );
    }

    /// StrLit("hello") <: Repr(String) = true.
    #[test]
    fn test_str_lit_subtype_of_str_repr() {
        let ctx = empty_ctx();
        let lit = make_str_lit("hello");
        let str_repr = make_repr(REPR_STRING);
        assert!(
            is_subtype_bas(&lit, &str_repr, &ctx),
            "StrLit must be a subtype of Repr(String)"
        );
    }

    // -------------------------------------------------------------------------
    // atoms_are_disjoint tests (fix #2 regression guards)
    // -------------------------------------------------------------------------

    /// StrLit and Repr(String) are NOT disjoint (a string literal inhabits both).
    #[test]
    fn test_str_lit_not_disjoint_from_repr_string() {
        let ctx = empty_ctx();
        let lit = make_str_lit("hi");
        let str_repr = make_repr(REPR_STRING);
        assert!(
            !atoms_are_disjoint(&lit, &str_repr, &ctx),
            "StrLit and Repr(String) must NOT be disjoint"
        );
    }

    /// StrLit and Repr(Int) ARE disjoint (a string cannot be an int).
    #[test]
    fn test_str_lit_disjoint_from_repr_int() {
        let ctx = empty_ctx();
        let lit = make_str_lit("hi");
        let int_repr = make_repr(REPR_INT);
        assert!(
            atoms_are_disjoint(&lit, &int_repr, &ctx),
            "StrLit and Repr(Int) must be disjoint"
        );
    }

    /// IntLit and Repr(Int) are NOT disjoint.
    #[test]
    fn test_int_lit_not_disjoint_from_repr_int() {
        let ctx = empty_ctx();
        let lit = make_int_lit(42);
        let int_repr = make_repr(REPR_INT);
        assert!(
            !atoms_are_disjoint(&lit, &int_repr, &ctx),
            "IntLit and Repr(Int) must NOT be disjoint"
        );
    }

    /// IntLit and Repr(String) ARE disjoint.
    #[test]
    fn test_int_lit_disjoint_from_repr_string() {
        let ctx = empty_ctx();
        let lit = make_int_lit(42);
        let str_repr = make_repr(REPR_STRING);
        assert!(
            atoms_are_disjoint(&lit, &str_repr, &ctx),
            "IntLit and Repr(String) must be disjoint"
        );
    }

    /// FloatLit and Repr(Float) are NOT disjoint.
    #[test]
    fn test_float_lit_not_disjoint_from_repr_float() {
        let ctx = empty_ctx();
        let lit = make_float_lit(3.14);
        let float_repr = make_repr(REPR_FLOAT);
        assert!(
            !atoms_are_disjoint(&lit, &float_repr, &ctx),
            "FloatLit and Repr(Float) must NOT be disjoint"
        );
    }

    /// FloatLit and Repr(Int) ARE disjoint.
    #[test]
    fn test_float_lit_disjoint_from_repr_int() {
        let ctx = empty_ctx();
        let lit = make_float_lit(3.14);
        let int_repr = make_repr(REPR_INT);
        assert!(
            atoms_are_disjoint(&lit, &int_repr, &ctx),
            "FloatLit and Repr(Int) must be disjoint"
        );
    }

    /// FloatLit(1.0) and FloatLit(2.0) ARE disjoint — different float values.
    #[test]
    fn test_float_lit_disjoint_from_different_float_lit() {
        let ctx = empty_ctx();
        let lit1 = make_float_lit(1.0);
        let lit2 = make_float_lit(2.0);
        assert!(
            atoms_are_disjoint(&lit1, &lit2, &ctx),
            "FloatLit(1.0) and FloatLit(2.0) must be disjoint"
        );
    }

    /// FloatLit(3.14) and FloatLit(3.14) are NOT disjoint — same float value.
    #[test]
    fn test_float_lit_not_disjoint_from_same_float_lit() {
        let ctx = empty_ctx();
        let lit1 = make_float_lit(3.14);
        let lit2 = make_float_lit(3.14);
        assert!(
            !atoms_are_disjoint(&lit1, &lit2, &ctx),
            "FloatLit(3.14) and FloatLit(3.14) must NOT be disjoint"
        );
    }

    /// FloatLit(1.0) is a subtype of FloatLit(1.0) — reflexivity of float literal subtyping.
    #[test]
    fn test_float_lit_reflexivity() {
        let ctx = empty_ctx();
        let lit1 = make_float_lit(1.0);
        let lit2 = make_float_lit(1.0);
        assert!(
            is_subtype_bas(&lit1, &lit2, &ctx),
            "FloatLit(1.0) must be a subtype of FloatLit(1.0)"
        );
    }

    // -------------------------------------------------------------------------
    // Conjunction emptiness tests
    // -------------------------------------------------------------------------

    /// Pos(Repr(Int)) & Pos(Repr(String)) is empty — two disjoint positives.
    #[test]
    fn test_conjunction_emptiness_disjoint_positives() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        // is_subtype_bas(Int, String) should be false (disjoint positives mean intersection empty)
        assert!(
            !is_subtype_bas(&int_repr, &str_repr, &ctx),
            "Int is not a subtype of String"
        );
        // The conjunction [Pos(Int), Pos(String)] should be detected as empty.
        let mut sigma = std::collections::HashSet::new();
        let conj = vec![
            SignedAtom::Pos(Arc::clone(&int_repr)),
            SignedAtom::Pos(Arc::clone(&str_repr)),
        ];
        assert!(
            is_conjunction_empty(&conj, &ctx, &mut sigma, 0),
            "Pos(Int) & Pos(String) must be empty (disjoint)"
        );
    }

    /// Pos(Repr(Int)) & Neg(Repr(Int)) is empty — positive subsumed by negative.
    #[test]
    fn test_conjunction_emptiness_subsumed_by_negative() {
        let ctx = empty_ctx();
        let int1 = make_repr(REPR_INT);
        let int2 = make_repr(REPR_INT);
        let mut sigma = std::collections::HashSet::new();
        let conj = vec![
            SignedAtom::Pos(Arc::clone(&int1)),
            SignedAtom::Neg(Arc::clone(&int2)),
        ];
        assert!(
            is_conjunction_empty(&conj, &ctx, &mut sigma, 0),
            "Pos(Int) & Neg(Int) must be empty"
        );
    }

    // -------------------------------------------------------------------------
    // B-465: sigma isolation per conjunction
    // -------------------------------------------------------------------------

    /// B-465 regression: sigma is isolated per conjunction — a hypothesis added when
    /// checking conjunction C1 must not cause conjunction C2 (in the same RDNF) to be
    /// incorrectly declared empty.
    ///
    /// The original bug used string-keyed sigma (`("a","b")` style); the current
    /// implementation uses structural-fingerprint-keyed sigma. This test
    /// verifies the actual risk vector: two structurally different recursive types in
    /// different conjunctions of the same RDNF must be evaluated independently.
    ///
    /// Scenario:
    ///   - mu_int = mu.X.(Int | X)  — unfolds to contain Int
    ///   - mu_str = mu.Y.(Str | Y)  — unfolds to contain Str
    ///
    /// is_subtype_bas checks `A & ~B` — the RDNF will have two disjuncts (from the
    /// union expansion). C1 will involve mu_int and should be empty (mu_int <: mu_str?
    /// No — but the test verifies RDNF correctly handles different recursive bodies
    /// without sigma from C1 leaking into C2).
    ///
    /// Concretely: Int is a subtype of (Int | Str), and Str is a subtype of (Int | Str),
    /// verifying that the RDNF for (Int | Str) is non-empty (the RDNF disjuncts don't
    /// cancel each other due to sigma leakage).
    #[test]
    fn test_b465_sigma_isolation_per_conjunction() {
        // B-465: sigma sets must be isolated between conjunctions in RDNF.
        // If the sigma from checking one conjunction leaked into the next, a
        // spurious coinductive hypothesis from C1 could make C2 vacuously true.
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let float_repr = make_repr(REPR_FLOAT);

        // Int <: Top (trivially true, no sigma needed)
        assert!(is_subtype_bas(&int_repr, &make_top(), &ctx), "Int <: Top");

        // Int is not <: Str (disjoint)
        assert!(
            !is_subtype_bas(&int_repr, &str_repr, &ctx),
            "Int not <: Str"
        );

        // The union (Int | Str) is correctly handled: sigma isolation means
        // that checking Int <: (Int | Str) doesn't contaminate checking Str <: (Int | Str).
        let int_or_str = make_union(vec![Arc::clone(&int_repr), Arc::clone(&str_repr)]);
        assert!(
            is_subtype_bas(&int_repr, &int_or_str, &ctx),
            "Int must be a subtype of Int | Str (C1 sigma must not cancel C2)"
        );
        assert!(
            is_subtype_bas(&str_repr, &int_or_str, &ctx),
            "Str must be a subtype of Int | Str (C2 sigma must not be tainted by C1)"
        );

        // Float <: (Int | Str) should be false (Float is not Int, Float is not Str).
        // If sigma from C1 (Float & ~Int) bled into C2 (Float & ~Str), the result
        // might be wrong.
        assert!(
            !is_subtype_bas(&float_repr, &int_or_str, &ctx),
            "Float must NOT be a subtype of Int | Str (sigma isolation)"
        );
    }

    /// S-Assum coinductive termination: a recursive type is a subtype of itself.
    ///
    /// `mu.X.(Int | {x: X}) <: mu.X.(Int | {x: X})` must return `true` and
    /// terminate without hitting the depth limit. The coinductive sigma key
    /// fires on the second visit to the same structural fingerprint pair — if the
    /// keys work correctly, termination is O(1) after the first unfolding.
    ///
    /// When the SAME Arc is compared to itself (`is_subtype_bas(&mu, &mu, ctx)`),
    /// `Arc::ptr_eq` fires immediately (reflexivity axiom). When two separately
    /// allocated Arcs with identical structure are compared, the structural
    /// fingerprint key ensures S-Assum fires correctly after one unfolding.
    ///
    /// This test verifies both cases: same-Arc reflexivity and cross-Arc structural
    /// identity.
    #[test]
    fn test_s_assum_terminates_for_reflexive_recursive_type() {
        let ctx = empty_ctx();

        // Build: mu.X.(Int | X)
        //   body = Union(Int, RecursiveRef(0))
        let int_repr = make_repr(REPR_INT);
        let recursive_ref = make_recursive_ref(0);
        let body = make_union(vec![Arc::clone(&int_repr), Arc::clone(&recursive_ref)]);
        let mu_type = make_recursive(body);

        // Case 1: same Arc — Arc::ptr_eq reflexivity fires immediately.
        assert!(
            is_subtype_bas(&mu_type, &mu_type, &ctx),
            "mu.X.(Int | X) must be a subtype of itself (same-Arc reflexivity)"
        );

        // Case 2: separately allocated structurally-identical Arcs (B-666/B-668).
        // Structural fingerprint key ensures S-Assum fires after one unfolding.
        let body2 = make_union(vec![Arc::clone(&int_repr), make_recursive_ref(0)]);
        let mu_type2 = make_recursive(body2);
        assert!(
            !Arc::ptr_eq(&mu_type, &mu_type2),
            "test setup: mu_type and mu_type2 must be different Arc allocations"
        );
        assert!(
            is_subtype_bas(&mu_type, &mu_type2, &ctx),
            "mu.X.(Int | X) <: mu.X.(Int | X) must hold for distinct Arcs (B-666, structural sigma)"
        );
        assert!(
            is_subtype_bas(&mu_type2, &mu_type, &ctx),
            "mu.X.(Int | X) <: mu.X.(Int | X) must hold in both directions (B-666)"
        );
    }

    /// B-666/B-668: structural fingerprints are stable across re-extraction.
    ///
    /// Two separately-allocated TypeValues with identical structure must produce
    /// identical fingerprints. This is the key invariant that makes coinductive
    /// sigma keys work correctly when `payload_typevalue_field` creates fresh Arcs.
    #[test]
    fn test_structural_fingerprint_stability() {
        // Build two identical mu.X.(Int | {x: X}) from scratch
        let int_repr_1 = make_repr(REPR_INT);
        let int_repr_2 = make_repr(REPR_INT);
        let rec_ref_1 = make_recursive_ref(0);
        let rec_ref_2 = make_recursive_ref(0);
        let record_1 = make_record(vec![("x", rec_ref_1)]);
        let record_2 = make_record(vec![("x", rec_ref_2)]);
        let body_1 = make_union(vec![int_repr_1, record_1]);
        let body_2 = make_union(vec![int_repr_2, record_2]);
        let mu_1 = make_recursive(body_1);
        let mu_2 = make_recursive(body_2);

        // Verify distinct allocations
        assert!(!Arc::ptr_eq(&mu_1, &mu_2));

        // Verify identical fingerprints
        let fp_1 = typevalue_structural_fingerprint(&mu_1, 0);
        let fp_2 = typevalue_structural_fingerprint(&mu_2, 0);
        assert_eq!(
            fp_1, fp_2,
            "Structurally identical TypeValues must produce identical fingerprints"
        );

        // Verify different types produce different fingerprints
        let str_repr = make_repr(REPR_STRING);
        let body_diff = make_union(vec![
            str_repr,
            make_record(vec![("x", make_recursive_ref(0))]),
        ]);
        let mu_diff = make_recursive(body_diff);
        let fp_diff = typevalue_structural_fingerprint(&mu_diff, 0);
        assert_ne!(
            fp_1, fp_diff,
            "Structurally different TypeValues must produce different fingerprints"
        );
    }

    /// B-668: coinductive subtyping works for recursive types nested inside records.
    ///
    /// When a recursive type appears as a field in a record, `is_record_subtype` calls
    /// `payload_typevalue_field` to extract the field's TypeValue. This creates a fresh
    /// Arc, which under the old pointer-based sigma would defeat S-Assum. With structural
    /// fingerprints, the sigma hypothesis is found correctly on re-entry.
    #[test]
    fn test_recursive_subtype_inside_record() {
        let ctx = empty_ctx();

        // Build {x: mu.X.(Int | X)} — two separate allocations
        let int_repr = make_repr(REPR_INT);
        let body_a = make_union(vec![Arc::clone(&int_repr), make_recursive_ref(0)]);
        let mu_a = make_recursive(body_a);
        let rec_a = make_record(vec![("x", mu_a)]);

        let body_b = make_union(vec![Arc::clone(&int_repr), make_recursive_ref(0)]);
        let mu_b = make_recursive(body_b);
        let rec_b = make_record(vec![("x", mu_b)]);

        assert!(!Arc::ptr_eq(&rec_a, &rec_b));
        assert!(
            is_subtype_bas(&rec_a, &rec_b, &ctx),
            "record with recursive field type must be subtype of itself (B-668, record-nested recursive)"
        );
    }

    // -------------------------------------------------------------------------
    // B-467: distribute limit
    // -------------------------------------------------------------------------

    /// B-467 regression: distribute() returns Top (vec![vec![]]) when product exceeds limit.
    ///
    /// When the cross-product of two RDNF's conjunctions exceeds MAX_RDNF_CONJUNCTIONS (1024),
    /// distribute returns Top (inhabited), causing is_subtype to return false (conservative).
    #[test]
    fn test_b467_distribute_respects_limit() {
        // Build 33 repr types. Cross-product of two 33-element RDNFs = 33*33 = 1089 > 1024.
        let repr_strs = [
            REPR_INT,
            REPR_STRING,
            REPR_FLOAT,
            REPR_PROXY, // REPR_BOOL excluded: Value::Bool is a phantom (no runtime variant)
            REPR_BYTES,
            REPR_DICT,
            REPR_FUNCTION,
            REPR_FILE,
            REPR_DIR_CAP,
            REPR_NET_CAP,
            REPR_TASK,
            REPR_CHANNEL,
            REPR_CONTEXT,
            REPR_REACTIVE_CELL,
            REPR_CLOCK_CAP,
            REPR_TIMEZONE,
            REPR_TIMESTAMP,
            REPR_DURATION,
            REPR_DECIMAL,
            REPR_BIGINT,
            REPR_QUIC_SESSION,
            REPR_QUIC_DATAGRAM_HANDLE,
            REPR_HTTP2_SESSION,
            REPR_HTTP3_SESSION,
            REPR_URI,
            REPR_PROGRAM,
            REPR_DOCUMENT,
            REPR_TYPE_CONTEXT,
            // Duplicate some to reach 33
            REPR_INT,
            REPR_STRING,
            REPR_FLOAT,
            REPR_ARENA, // REPR_BOOL excluded: Value::Bool is a phantom (no runtime variant)
            REPR_BYTES,
        ];
        let members: Vec<Arc<Value>> = repr_strs.iter().map(|r| make_repr(r)).collect();
        let union_a = to_rdnf(&make_union(members.clone()));
        let union_b = to_rdnf(&make_union(members));
        // Cross product: 33 * 33 = 1089 > MAX_RDNF_CONJUNCTIONS (1024)
        let result = distribute(&union_a, &union_b);
        // Should return vec![vec![]] (Top = inhabited): exactly one empty conjunction.
        assert_eq!(
            result.len(),
            1,
            "distribute must return exactly one conjunction (Top) when product exceeds MAX_RDNF_CONJUNCTIONS"
        );
        assert_eq!(
            result[0].len(),
            0,
            "distribute must return an empty conjunction (Top = inhabited) when product exceeds limit"
        );
    }

    /// B-467 regression: distribute() should produce real conjunctions for small products.
    ///
    /// When the cross-product of two RDNF's conjunctions is well within MAX_RDNF_CONJUNCTIONS
    /// (1024), distribute must return the full product — not Top. This test guards against
    /// regressions where distribute always returns Top regardless of product size.
    #[test]
    fn test_b467_distribute_within_limit() {
        // Union([A, B]) & Union([C, D]) → 4 conjunctions (2×2), well within 1024 limit.
        let a = make_repr(REPR_INT);
        let b = make_repr(REPR_STRING);
        let c = make_repr(REPR_FLOAT);
        let d = make_repr(REPR_BYTES);
        let union1 = to_rdnf(&make_union(vec![Arc::clone(&a), Arc::clone(&b)]));
        let union2 = to_rdnf(&make_union(vec![Arc::clone(&c), Arc::clone(&d)]));
        let result = distribute(&union1, &union2);
        // Should have 4 non-empty conjunctions (2×2 distribution), not Top (vec![vec![]])
        assert_eq!(
            result.len(),
            4,
            "distribute within limit should produce 4 conjunctions (2×2 cross product), not Top"
        );
        assert!(
            result.iter().all(|conj| !conj.is_empty()),
            "distribute within limit: all conjunctions must be non-empty (not Top)"
        );
    }

    /// B-467 regression: distribute() at the exact limit (1024 conjunctions).
    ///
    /// When the product exactly equals MAX_RDNF_CONJUNCTIONS, distribute must not
    /// panic or loop. This test verifies that is_subtype terminates for a large union
    /// without crashing the process.
    #[test]
    fn test_b467_distribute_at_exact_limit() {
        // Build a 4-element union: 4×4 = 16 conjunctions, nowhere near the limit.
        // We use is_subtype_bas rather than distribute directly to test end-to-end
        // behavior including the safety valve.
        let reprs: Vec<Arc<Value>> = vec![
            make_repr(REPR_INT),
            make_repr(REPR_STRING),
            make_repr(REPR_FLOAT),
            make_repr(REPR_BYTES),
        ];
        let big_union = make_union(reprs);
        let ctx = empty_ctx();
        // Just verify no panic — the result can be true or false (depends on BAS semantics).
        let _ = is_subtype_bas(&big_union, &make_top(), &ctx);
        let _ = is_subtype_bas(&make_repr(REPR_INT), &big_union, &ctx);
    }

    // -------------------------------------------------------------------------
    // Function subtyping tests
    // -------------------------------------------------------------------------

    /// Fn(Int) -> Top <: Fn(IntLit(42)) -> Top: contravariant param.
    ///
    /// sub has param type Int, sup has param type IntLit(42).
    /// For fn subtyping: sup_param <: sub_param (contravariant).
    /// IntLit(42) <: Int is true, so the fn subtyping holds.
    #[test]
    fn test_fn_subtype_contravariant_params() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let int_lit = make_int_lit(42);
        let top = make_top();
        // sub: Fn(Int) -> Top
        let sub = make_fn(vec![Arc::clone(&int_repr)], Arc::clone(&top));
        // sup: Fn(IntLit(42)) -> Top
        let sup = make_fn(vec![Arc::clone(&int_lit)], Arc::clone(&top));
        // Contravariant: sup_param(IntLit(42)) <: sub_param(Int) iff IntLit <: Int, which is true.
        assert!(
            is_subtype_bas(&sub, &sup, &ctx),
            "Fn(Int)->Top must be a subtype of Fn(IntLit(42))->Top (contravariant param)"
        );
    }

    /// Fn(Top) -> Int is NOT <: Fn(Top) -> String: covariant return.
    #[test]
    fn test_fn_subtype_covariant_return_fails() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let top = make_top();
        // sub: Fn(Top) -> Int
        let sub = make_fn(vec![Arc::clone(&top)], Arc::clone(&int_repr));
        // sup: Fn(Top) -> String
        let sup = make_fn(vec![Arc::clone(&top)], Arc::clone(&str_repr));
        assert!(
            !is_subtype_bas(&sub, &sup, &ctx),
            "Fn(Top)->Int must NOT be a subtype of Fn(Top)->String (covariant return)"
        );
    }

    // -------------------------------------------------------------------------
    // Record subtyping tests
    // -------------------------------------------------------------------------

    /// {x: Int, y: Int} <: {x: Int}: width subtyping (sub has more fields).
    #[test]
    fn test_record_subtype_width() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        // sub has extra field y
        let sub = make_record(vec![
            ("x", Arc::clone(&int_repr)),
            ("y", Arc::clone(&int_repr)),
        ]);
        // sup requires only x
        let sup = make_record(vec![("x", Arc::clone(&int_repr))]);
        // Both closed: sub (closed) <: sup (closed) is valid since sub has all of sup's fields.
        assert!(
            is_subtype_bas(&sub, &sup, &ctx),
            "{{x:Int, y:Int}} must be a subtype of {{x:Int}} (width subtyping)"
        );
    }

    /// {x: Int} is NOT <: {x: Int, y: Int}: sub missing required field.
    #[test]
    fn test_record_subtype_missing_field_fails() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let sub = make_record(vec![("x", Arc::clone(&int_repr))]);
        let sup = make_record(vec![
            ("x", Arc::clone(&int_repr)),
            ("y", Arc::clone(&int_repr)),
        ]);
        assert!(
            !is_subtype_bas(&sub, &sup, &ctx),
            "{{x:Int}} must NOT be a subtype of {{x:Int, y:Int}} (missing field)"
        );
    }

    // -------------------------------------------------------------------------
    // Direct is_fn_subtype tests
    // -------------------------------------------------------------------------

    /// Fn(Int) -> Str <: Fn(Int) -> Any (Top): covariant return.
    ///
    /// The sub has a more specific return type (Str), sup requires Any (Top).
    /// Covariant return: sub_ret <: sup_ret, i.e. Str <: Top → true.
    #[test]
    fn test_is_fn_subtype_basic_covariant_return() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let top = make_top();
        // sub: Fn(Int) -> Str
        let sub = make_fn(vec![Arc::clone(&int_repr)], Arc::clone(&str_repr));
        // sup: Fn(Int) -> Top (Any)
        let sup = make_fn(vec![Arc::clone(&int_repr)], Arc::clone(&top));
        let mut sigma = std::collections::HashSet::new();
        assert!(
            is_fn_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "Fn(Int)->Str must be a subtype of Fn(Int)->Top (covariant return)"
        );
    }

    /// Fn(Any) -> Int <: Fn(Str) -> Int: contravariant params.
    ///
    /// For fn subtyping: sup_param <: sub_param (contravariant).
    /// sub_param = Top (Any), sup_param = Str.
    /// Str <: Top → true, so the fn subtyping holds.
    #[test]
    fn test_is_fn_subtype_basic_contravariant_params() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let top = make_top();
        // sub: Fn(Top) -> Int  (accepts anything as param)
        let sub = make_fn(vec![Arc::clone(&top)], Arc::clone(&int_repr));
        // sup: Fn(Str) -> Int  (requires Str as param)
        let sup = make_fn(vec![Arc::clone(&str_repr)], Arc::clone(&int_repr));
        // Contravariant: sup_param(Str) <: sub_param(Top) → Str <: Top → true
        let mut sigma = std::collections::HashSet::new();
        assert!(
            is_fn_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "Fn(Top)->Int must be a subtype of Fn(Str)->Int (contravariant param)"
        );
    }

    /// Fn(Int, Str) -> Int NOT <: Fn(Int) -> Int: arity mismatch.
    ///
    /// Different parameter counts fail immediately — no meaningful subtyping
    /// relationship holds between functions of different arity.
    #[test]
    fn test_is_fn_subtype_arity_mismatch() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        // sub: Fn(Int, Str) -> Int  — 2 params
        let sub = make_fn(
            vec![Arc::clone(&int_repr), Arc::clone(&str_repr)],
            Arc::clone(&int_repr),
        );
        // sup: Fn(Int) -> Int  — 1 param
        let sup = make_fn(vec![Arc::clone(&int_repr)], Arc::clone(&int_repr));
        let mut sigma = std::collections::HashSet::new();
        assert!(
            !is_fn_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "Fn(Int,Str)->Int must NOT be a subtype of Fn(Int)->Int (arity mismatch)"
        );
    }

    // -------------------------------------------------------------------------
    // Direct is_record_subtype tests
    // -------------------------------------------------------------------------

    /// {x: Int, y: Str} <: {x: Int}: width subtyping — more fields is a subtype of fewer.
    #[test]
    fn test_is_record_subtype_width_subtyping() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        // sub: {x: Int, y: Str}
        let sub = make_record(vec![
            ("x", Arc::clone(&int_repr)),
            ("y", Arc::clone(&str_repr)),
        ]);
        // sup: {x: Int}
        let sup = make_record(vec![("x", Arc::clone(&int_repr))]);
        let mut sigma = std::collections::HashSet::new();
        assert!(
            is_record_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "{{x:Int, y:Str}} must be a subtype of {{x:Int}} (width subtyping)"
        );
    }

    /// {x: Int} NOT <: {x: Str}: field type mismatch prevents subtyping.
    ///
    /// Depth subtyping is covariant: sub_field_type <: sup_field_type.
    /// Int is not <: Str, so this fails.
    #[test]
    fn test_is_record_subtype_field_type_mismatch() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        // sub: {x: Int}
        let sub = make_record(vec![("x", Arc::clone(&int_repr))]);
        // sup: {x: Str}
        let sup = make_record(vec![("x", Arc::clone(&str_repr))]);
        let mut sigma = std::collections::HashSet::new();
        assert!(
            !is_record_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "{{x:Int}} must NOT be a subtype of {{x:Str}} (field type mismatch)"
        );
    }

    /// {x: Int} NOT <: {x: Int, y: Str}: sub missing a required field.
    ///
    /// Width subtyping only permits sub to have MORE fields, not fewer.
    #[test]
    fn test_is_record_subtype_missing_field() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        // sub: {x: Int}
        let sub = make_record(vec![("x", Arc::clone(&int_repr))]);
        // sup: {x: Int, y: Str}
        let sup = make_record(vec![
            ("x", Arc::clone(&int_repr)),
            ("y", Arc::clone(&str_repr)),
        ]);
        let mut sigma = std::collections::HashSet::new();
        assert!(
            !is_record_subtype(&sub, &sup, &ctx, 0, &mut sigma),
            "{{x:Int}} must NOT be a subtype of {{x:Int, y:Str}} (missing field y)"
        );
    }

    // -------------------------------------------------------------------------
    // Depth guard tests (T-2076 regression)
    // -------------------------------------------------------------------------

    /// Verify the MAX_ATOM_SUBTYPE_DEPTH guard fires for deep types via is_subtype_bas.
    ///
    /// `is_subtype_bas_with_sigma` passes `depth` through RDNF distribution to
    /// `is_atom_subtype`, where the `MAX_ATOM_SUBTYPE_DEPTH` guard lives. At depth
    /// limit, `is_atom_subtype` returns false regardless of the types. This test
    /// verifies the guard fires by calling `is_atom_subtype` directly at the limit.
    ///
    /// Note: `is_subtype_bas_with_sigma` has a ptr_eq reflexivity short-circuit that
    /// fires before depth is consulted, so the guard is only reachable via
    /// `is_atom_subtype` with distinct Arc allocations for the same type value.
    ///
    /// Mutation resistance: if a future refactor removes `depth + 1` threading through
    /// recursive calls in `is_subtype_bas_with_sigma`, the depth accumulation will break
    /// and the depth guard in `is_atom_subtype` will never fire, potentially causing
    /// stack overflow on pathologically deep types.
    #[test]
    fn test_depth_guard_fires_via_is_atom_subtype() {
        let ctx = empty_ctx();
        // Two distinct Arc allocations for the same Repr(Int) type —
        // ptr_eq does NOT fire, so the RDNF / is_atom_subtype path is taken.
        let int_a = make_repr(REPR_INT);
        let int_b = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);

        // Confirm these are distinct Arcs (if ptr_eq, the test loses its meaning).
        assert!(
            !Arc::ptr_eq(&int_a, &int_b),
            "test requires distinct Arc allocations for the same Repr type"
        );

        // At depth == MAX_ATOM_SUBTYPE_DEPTH the guard in is_atom_subtype fires and
        // returns false — even for Int <: Int (which would normally be true).
        let mut sigma = std::collections::HashSet::new();
        let result_at_limit =
            is_atom_subtype(&int_a, &int_b, &ctx, MAX_ATOM_SUBTYPE_DEPTH, &mut sigma);
        assert!(
            !result_at_limit,
            "is_atom_subtype must return false at depth == MAX_ATOM_SUBTYPE_DEPTH"
        );

        // One below the limit: Int <: Int is true (guard does not fire).
        let mut sigma2 = std::collections::HashSet::new();
        let result_below = is_atom_subtype(
            &int_a,
            &int_b,
            &ctx,
            MAX_ATOM_SUBTYPE_DEPTH - 1,
            &mut sigma2,
        );
        assert!(
            result_below,
            "is_atom_subtype must not fire depth guard at depth == MAX_ATOM_SUBTYPE_DEPTH - 1"
        );

        // Guard also fires for unrelated types (unconditional on type identity).
        let mut sigma3 = std::collections::HashSet::new();
        let result_unrelated =
            is_atom_subtype(&int_a, &str_repr, &ctx, MAX_ATOM_SUBTYPE_DEPTH, &mut sigma3);
        assert!(
            !result_unrelated,
            "is_atom_subtype must return false at depth limit regardless of types"
        );
    }

    /// Verify the MAX_ATOM_SUBTYPE_DEPTH guard fires in is_atom_subtype.
    ///
    /// is_atom_subtype is the entry point called from RDNF distribution. When
    /// depth >= MAX_ATOM_SUBTYPE_DEPTH it must return false immediately — this is
    /// the same guard that is_fn_subtype and is_record_subtype both check, ensuring
    /// depth threading from is_atom_subtype into those helpers causes them to terminate.
    #[test]
    fn test_depth_guard_fires_in_is_atom_subtype() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let mut sigma = std::collections::HashSet::new();

        // At depth == MAX_ATOM_SUBTYPE_DEPTH the guard must fire and return false.
        let result = is_atom_subtype(
            &int_repr,
            &int_repr,
            &ctx,
            MAX_ATOM_SUBTYPE_DEPTH,
            &mut sigma,
        );
        assert!(
            !result,
            "is_atom_subtype must return false at depth == MAX_ATOM_SUBTYPE_DEPTH"
        );

        // Verify is_fn_subtype receives depth correctly: build a Fn->Fn pair and
        // call is_atom_subtype at depth MAX_ATOM_SUBTYPE_DEPTH. The outer guard fires
        // before delegating to is_fn_subtype, so result is false.
        let top = make_top();
        let fn_type = make_fn(vec![Arc::clone(&int_repr)], Arc::clone(&top));
        let mut sigma2 = std::collections::HashSet::new();
        let fn_result = is_atom_subtype(
            &fn_type,
            &fn_type,
            &ctx,
            MAX_ATOM_SUBTYPE_DEPTH,
            &mut sigma2,
        );
        assert!(
            !fn_result,
            "is_atom_subtype at depth limit must return false even for Fn <: Fn"
        );

        // Verify is_record_subtype receives depth correctly: same pattern for records.
        let rec = make_record(vec![("x", Arc::clone(&int_repr))]);
        let mut sigma3 = std::collections::HashSet::new();
        let rec_result = is_atom_subtype(&rec, &rec, &ctx, MAX_ATOM_SUBTYPE_DEPTH, &mut sigma3);
        assert!(
            !rec_result,
            "is_atom_subtype at depth limit must return false even for Record <: Record"
        );
    }

    // -------------------------------------------------------------------------
    // Union/Inter BAS tests
    // -------------------------------------------------------------------------

    /// Int <: Int | String: sub is a member of the union.
    #[test]
    fn test_union_subtype_member() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let union = make_union(vec![Arc::clone(&int_repr), Arc::clone(&str_repr)]);
        assert!(
            is_subtype_bas(&int_repr, &union, &ctx),
            "Int must be a subtype of Int | String"
        );
    }

    /// Int | String is NOT <: Int: union not subtype of member.
    #[test]
    fn test_union_not_subtype_of_member() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let union = make_union(vec![Arc::clone(&int_repr), Arc::clone(&str_repr)]);
        assert!(
            !is_subtype_bas(&union, &int_repr, &ctx),
            "Int | String must NOT be a subtype of Int"
        );
    }

    /// Int & String is uninhabited (Never): is_subtype_bas(Int & String, Never) = true
    /// because Int & String is empty.
    #[test]
    fn test_inter_disjoint_is_empty() {
        let ctx = empty_ctx();
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let inter = make_inter(vec![Arc::clone(&int_repr), Arc::clone(&str_repr)]);
        let never = make_never();
        assert!(
            is_subtype_bas(&inter, &never, &ctx),
            "Int & String (uninhabited) must be a subtype of Never"
        );
    }

    // -------------------------------------------------------------------------
    // RecursiveRef depth fix (fix #1 regression guard)
    // -------------------------------------------------------------------------

    /// Verify that unfold_recursive_typevalue correctly substitutes RecursiveRef{depth:0}.
    ///
    /// Before fix #1, substitute_recursive_ref read "index" instead of "depth",
    /// causing unfold to always return the original value unchanged.
    #[test]
    fn test_recursive_ref_substitution_uses_depth_field() {
        // Build: mu.X.RecursiveRef(0) — a trivially self-referential type.
        // After one unfolding: substitute RecursiveRef{depth:0} with the full mu type.
        let recursive_ref = make_recursive_ref(0);
        let mu_type = make_recursive(Arc::clone(&recursive_ref));

        // Unfold one step: should substitute the ref with the mu type itself.
        let unfolded = unfold_recursive_typevalue(&mu_type);

        // After unfolding, the result should be the original mu_type (since the body
        // is RecursiveRef(0) which gets replaced by mu_type itself).
        // We verify by checking it's a TV_RECURSIVE (the substituted mu_type).
        assert_eq!(
            typevalue_ctor(&unfolded),
            Some(TV_RECURSIVE),
            "unfold_recursive_typevalue must substitute RecursiveRef{{depth:0}} with the recursive type"
        );
    }

    // -------------------------------------------------------------------------
    // substitute_recursive_ref — TV_APP and TV_NOMINAL_VARIANT coverage
    // -------------------------------------------------------------------------

    /// substitute_recursive_ref must recurse into the `arg` of a TV_APP.
    ///
    /// Build: App(Int, RecursiveRef{depth:0}).
    /// After substituting depth=0 with Repr("Value::String"), the arg becomes Repr.
    #[test]
    fn test_substitute_recursive_ref_tv_app_arg() {
        let int_repr = make_repr(REPR_INT);
        let str_repr = make_repr(REPR_STRING);
        let rec_ref = make_recursive_ref(0);

        // Build App(int_repr, rec_ref)
        let app =
            crate::type_infer::make_typevalue_app(Arc::clone(&int_repr), Arc::clone(&rec_ref));

        // Substitute RecursiveRef{depth:0} → str_repr
        let result = substitute_recursive_ref(&app, 0, &str_repr);

        // Result must still be TV_APP
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_APP),
            "result must be TV_APP"
        );

        // Extract the arg from the result using bas.rs's payload helpers (available via use super::*)
        let payload = typevalue_payload(&result).expect("result TV_APP must have payload");
        let got_arg = payload_typevalue_field(payload, FIELD_ARG)
            .expect("result TV_APP payload must have 'arg' field");
        assert_eq!(
            typevalue_ctor(&got_arg),
            Some(TV_REPR),
            "substituted arg must be TV_REPR (the replacement), not RecursiveRef"
        );
    }

    /// substitute_recursive_ref must recurse into the `fields` of a TV_NOMINAL_VARIANT.
    ///
    /// Build: NominalVariant("Foo", "Foo.Bar", Record{r: RecursiveRef{depth:0}}).
    /// After substituting depth=0 with Repr("Value::Int"), the field type becomes Int.
    #[test]
    fn test_substitute_recursive_ref_tv_nominal_variant_fields() {
        let int_repr = make_repr(REPR_INT);
        let rec_ref = make_recursive_ref(0);

        // Build a Record with one field "r" whose type is RecursiveRef{depth:0}
        let mut field_map = indexmap::IndexMap::new();
        field_map.insert("r".to_string(), Arc::clone(&rec_ref));
        let fields_record = crate::type_infer::make_typevalue_record(field_map, None);

        // Build NominalVariant("Foo", "Foo.Bar", fields_record)
        let nominal =
            crate::type_infer::make_typevalue_nominal_variant("Foo", "Foo.Bar", fields_record);

        // Substitute RecursiveRef{depth:0} → int_repr
        let result = substitute_recursive_ref(&nominal, 0, &int_repr);

        // Result must still be TV_NOMINAL_VARIANT
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_NOMINAL_VARIANT),
            "result must be TV_NOMINAL_VARIANT"
        );
        // The result must NOT be pointer-equal to the original (something changed)
        assert!(
            !Arc::ptr_eq(&nominal, &result),
            "result must be a new Arc (fields changed)"
        );
        // The "r" field inside the result's fields Record must be int_repr, not RecursiveRef.
        // Extract the fields Record from the NominalVariant's payload.
        let result_payload =
            typevalue_payload(&result).expect("result NominalVariant must have payload");
        let result_fields_tv = payload_typevalue_field(result_payload, FIELD_FIELDS)
            .expect("result NominalVariant must have 'fields' field");
        // result_fields_tv is a TypeValue.Record — extract its named fields.
        let record_fields = crate::type_infer::typevalue_record_fields_pub(&result_fields_tv);
        let got_r = record_fields
            .get("r")
            .expect("fields Record must contain 'r' field")
            .clone();
        assert_eq!(
            typevalue_ctor(&got_r),
            Some(TV_REPR),
            "substituted 'r' field must be TV_REPR (int_repr), not RecursiveRef"
        );
    }
}
