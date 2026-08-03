//! Type class declarations, constraints, and class/instance environments.
//!
//! This module contains the type class system infrastructure including
//! `ClassDecl` and `InstanceDecl`.
//!
//! Constraints are represented as `Arc<Value>` TypeValues using the `ConstraintDecl`
//! variant declared in stdlib/builtin_core.llt:
//!
//! ```
//! ConstraintDecl: [type
//!   class: TypeValue
//!   args:  Dict]
//! ```
//!
//! A class constraint `Sortable a` becomes:
//! ```
//! Value::Variant { ctor: "ConstraintDecl", payload: Dict { class: <Sortable TypeValue>, args: { "0": TypeValue.Var "a" } } }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::type_tags::*;
use crate::value::{unknown_type_val, HashableValue, Thunk, Value};

/// Global counter for unique class declaration IDs.
/// Shared between the resolver's Phase 1b scan (which pre-assigns IDs before type-checking)
/// and the type-checker (which reads the pre-assigned ID from the AST node).
/// ID 0 is reserved as a placeholder for ClassDecl objects created before ID assignment.
static CLASS_DECL_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Returns the next globally unique class declaration ID.
/// Called by the resolver's Phase 1b scan to pre-assign IDs before type-checking runs,
/// and by the type-checker as a fallback when no pre-assigned ID exists.
pub fn next_class_decl_id() -> u64 {
    CLASS_DECL_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Structural discharge rule for a typeclass — a general mechanism for declaring
/// that a typeclass is satisfied by a structural property of the type rather than
/// by a registered instance. This avoids hardcoding class names in the constraint solver.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum StructuralDischarge {
    /// No structural rule — use normal instance resolution (the default).
    #[default]
    None,
    /// Only closed dicts satisfy this constraint.
    /// Open dicts do NOT satisfy it and produce a type error.
    /// Used by the `Record` typeclass to enforce closed-dict semantics.
    ClosedDict,
}

/// A TypeValue is an `Arc<Value>` where the Value is a `Value::Variant` with
/// a constructor tag like `"TypeValue.Int"`, `"TypeValue.Var"`, `"TypeValue.Fn"`, etc.
/// as declared in stdlib/builtin_core.llt.
pub type TypeValue = Arc<Value>;

// ─── Internal span helper ─────────────────────────────────────────────────────

/// Build an AST span for TypeValue bootstrap construction (no source location).
fn boot_span() -> Span {
    Span {
        file: Arc::from("<type_class>"),
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 0,
        name: None,
    }
}

/// Create a settled (already-forced) thunk wrapping a Value.
pub(crate) fn settled_thunk(v: Value) -> Arc<Thunk> {
    Arc::new(Thunk::value(v, boot_span()))
}

// ─── TypeValue construction helpers ───────────────────────────────────────────

/// Build a `Value::String` from a plain `&str`.
fn string_value(s: &str) -> Value {
    let source: Arc<str> = Arc::from(s);
    let end = source.len();
    Value::String {
        source,
        start: 0,
        end,
        type_val: unknown_type_val(),
    }
}

/// Build a single-field payload Dict.
fn single_field_dict(key: &str, value: Value) -> Value {
    let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    entries.insert(HashableValue::Str(Arc::from(key)), settled_thunk(value));
    Value::Dict {
        entries,
        type_val: unknown_type_val(),
    }
}

/// Build a two-field payload Dict.
fn two_field_dict(key1: &str, val1: Value, key2: &str, val2: Value) -> Value {
    let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    entries.insert(HashableValue::Str(Arc::from(key1)), settled_thunk(val1));
    entries.insert(HashableValue::Str(Arc::from(key2)), settled_thunk(val2));
    Value::Dict {
        entries,
        type_val: unknown_type_val(),
    }
}

/// Construct a `Value::Variant` (the fundamental building block of TypeValues).
fn make_variant(ctor: &str, payload: Option<Value>) -> Value {
    Value::Variant {
        ctor: Arc::from(ctor),
        payload: payload.map(|p| settled_thunk(p)),
        type_val: unknown_type_val(),
        type_decl_id: 0,
    }
}

/// Make a `TypeValue.Op { name }` variant — a TypeValue representing a type operator.
///
/// Represents a type operator name (e.g., "HasField", "Seq", "Map").
pub fn make_type_op(name: impl Into<String>) -> TypeValue {
    let name_str: String = name.into();
    let payload = single_field_dict("name", string_value(&name_str));
    Arc::new(make_variant(TV_OP, Some(payload)))
}

/// Extract the inner `Value` from an `Arc<Value>`, cloning if there are multiple owners.
///
/// This is needed when constructing payload dicts where we need owned `Value` values
/// but are given `Arc<Value>` TypeValues.
pub(crate) fn arc_into_value(arc: Arc<Value>) -> Value {
    match Arc::try_unwrap(arc) {
        Ok(v) => v,
        Err(arc_ref) => (*arc_ref).clone(),
    }
}

/// Construct a `ConstraintDecl` Arc<Value> for a class constraint.
///
/// `class_tv` is the TypeValue representing the class (e.g., the TypeValue for "Sortable").
/// `args` is a Vec of argument TypeValues, in constraint-position order.
///
/// The resulting value is:
/// ```
/// Value::Variant { ctor: "ConstraintDecl", payload: { class: class_tv, args: { 0: arg0, 1: arg1, ... } } }
/// ```
pub fn make_constraint_decl(class_tv: TypeValue, args: Vec<TypeValue>) -> TypeValue {
    // Build args dict (auto-indexed by integer position)
    let mut args_entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(args.len());
    for (i, arg) in args.into_iter().enumerate() {
        args_entries.insert(
            HashableValue::Int(i as i64),
            settled_thunk(arc_into_value(arg)),
        );
    }
    let args_dict = Value::Dict {
        entries: args_entries,
        type_val: unknown_type_val(),
    };

    let class_val = arc_into_value(class_tv);
    let payload = two_field_dict("class", class_val, "args", args_dict);
    Arc::new(make_variant(TV_CONSTRAINT_DECL, Some(payload)))
}

// ─── TypeValue inspection helpers ─────────────────────────────────────────────

/// Extract the constructor tag from a TypeValue (Value::Variant).
/// Returns `None` if the value is not a Variant.
///
/// Delegates to the canonical definition in `crate::type_tags::typevalue_ctor`.
pub fn typevalue_ctor(tv: &Arc<Value>) -> Option<&str> {
    crate::type_tags::typevalue_ctor(tv)
}

/// Create a TypeValue.App (type application) — `op` applied to `arg`.
///
/// Produces `TypeValue.App { op: TypeValue, arg: TypeValue }`.
/// Used to represent parameterized types like `Seq Int` or `Result Str Err`.
/// Delegates to `crate::type_infer::make_typevalue_app` — the canonical construction site.
#[cfg(test)]
pub fn make_type_app(op: TypeValue, arg: TypeValue) -> TypeValue {
    crate::type_infer::make_typevalue_app(op, arg)
}

// ─── ClassDecl ────────────────────────────────────────────────────────────────

/// Type class declaration (Wadler & Blott 1989)
/// Example: `[class [Equatable a] eq: [Fn@Bool [a a]]]`
#[derive(Debug, Clone)]
pub struct ClassDecl {
    /// Unique ID assigned by the lowerer when processing each `[class ...]` declaration.
    /// Same mechanism as `type_decl_id` for nominal types. Enables stable class identity
    /// across scopes instead of relying on string name matching.
    pub class_decl_id: u64,
    /// Class name (e.g., "Equatable")
    pub name: String,
    /// Type parameters with their kind TypeValues.
    /// The kind TypeValue is one of:
    ///   - TypeValue.Op { name: "Type" } — proper type kind `*`
    ///   - TypeValue.Fn { params: [...], return: TypeValue.Op "Type" } — type constructor kind `* → *`
    ///   - TypeValue.Op { name: "Row" } — row kind
    ///   - TypeValue.Op { name: "Label" } — label kind
    pub params: Vec<(String, Arc<Value>)>,
    /// Superclass constraints as (class_name, Vec<param_names>) tuples.
    /// Example: ("Functor", vec!["f"]) means this class extends Functor with parameter f.
    pub superclasses: Vec<(String, Vec<String>)>,
    /// Functional dependencies: (determining_positions, determined_positions) pairs.
    /// Each pair is (Vec<usize>, Vec<usize>) indexing into `params`.
    /// Example: for Add a b c with FD (a,b) → c: determines = vec![(vec![0,1], vec![2])]
    pub(crate) determines: Vec<(Vec<usize>, Vec<usize>)>,
    /// Optional resolver: the name of the typeclass method (or builtin function) that,
    /// given ground values for the determining positions, computes the determined type.
    /// When Some, FD improvement uses the resolver to compute the target type rather than
    /// scanning registered instances. When None, instance lookup is used.
    pub resolver: Option<String>,
    /// Whether the resolver function is injective (one-to-one from source positions to result).
    /// If true, unifying two `TV_APP` applications of the resolver pairwise unifies their
    /// arguments (safe because injectivity means equal results ↔ equal args).
    /// If false, pairwise unification is deferred to `state.deferred_equalities` because
    /// the resolver may map different inputs to the same output.
    pub resolver_injective: bool,
    /// Structural discharge rule — enables this typeclass to be satisfied by a structural
    /// property without a registered instance.
    pub structural_discharge: StructuralDischarge,
    /// Method signatures declared in the class body.
    /// Each entry is (method_name, method_type_as_TypeValue) where method_type is a
    /// TypeValue.Fn Arc<Value> using the class's type parameters as TypeValue.Var nodes.
    pub method_signatures: Vec<(String, Arc<Value>)>,
}

impl PartialEq for ClassDecl {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for ClassDecl {}

impl std::hash::Hash for ClassDecl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl fmt::Display for ClassDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

// ─── InstanceDecl ─────────────────────────────────────────────────────────────

/// Type class instance declaration
/// Example: `[instance [Equatable NativeInt] eq: [fn [x y] [= x y]]]`
#[derive(Debug, Clone)]
pub struct InstanceDecl {
    /// Class name (e.g., "Equatable")
    pub class_name: String,
    /// Instance type as a TypeValue (Arc<Value>).
    /// For single-parameter classes: the type the instance covers
    ///   (e.g., TypeValue.Repr { repr: "Value::Int" } for NativeInt).
    /// For multi-parameter type classes, this is a TypeValue.Record with numbered fields:
    ///   `[Add NativeInt Float64 Float64]` →
    ///   TypeValue.Record { fields: { 0: NativeInt TypeValue, 1: Float64 TypeValue, 2: Float64 TypeValue }, tail: RowTail.Closed }
    pub instance_type: Arc<Value>,
    /// Determining positions (indices into the multi-param pattern) used to build the lookup key.
    /// Empty for single-parameter classes (no functional dependencies).
    /// Example: for `Add a b c` with FD `(a,b) → c`, this is `vec![0, 1]`.
    pub det_positions: Vec<usize>,
    /// Method implementations: method_name -> inferred type as TypeValue.
    /// (The actual runtime dictionary value is stored in eval::ClassDictionary)
    pub method_types: HashMap<String, Arc<Value>>,
}

// ─── FD improvement ───────────────────────────────────────────────────────────

/// Extract the class name and ordered arg TypeValues from a ConstraintDecl Arc<Value>.
///
/// Returns `None` if the constraint is not a ConstraintDecl, or if the payload cannot
/// be read synchronously (unsettled thunk). The args are returned in ascending integer
/// key order (position 0, 1, 2, ...).
fn extract_fd_constraint_parts(cv: &Arc<Value>) -> Option<(String, Vec<Arc<Value>>)> {
    match cv.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => {
            let payload_val = thunk.peek_result()?;
            let payload_dict = match payload_val {
                Ok(Value::Dict { entries, .. }) => entries,
                _ => return None,
            };
            // Extract class name from class: TypeValue.Op { name: ... }
            let class_key = HashableValue::Str(Arc::from(FIELD_CLASS));
            let class_thunk = payload_dict.get(&class_key)?;
            let class_name = match class_thunk.peek_result()? {
                Ok(Value::Variant {
                    ctor: c_ctor,
                    payload: Some(class_payload),
                    ..
                }) if c_ctor.as_ref() == TV_OP => {
                    let class_payload_val = class_payload.peek_result()?;
                    match class_payload_val {
                        Ok(Value::Dict { entries: inner, .. }) => {
                            let name_key = HashableValue::Str(Arc::from(FIELD_NAME));
                            let name_thunk = inner.get(&name_key)?;
                            match name_thunk.peek_result()? {
                                Ok(Value::String {
                                    source, start, end, ..
                                }) => source[*start..*end].to_string(),
                                _ => return None,
                            }
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            };
            // Extract args dict: { 0: TypeValue, 1: TypeValue, ... }
            let args_key = HashableValue::Str(Arc::from(FIELD_ARGS));
            let args_thunk = payload_dict.get(&args_key)?;
            let args_entries = match args_thunk.peek_result()? {
                Ok(Value::Dict { entries, .. }) => entries,
                _ => return None,
            };
            // Collect args in ascending integer key order.
            let mut indexed: Vec<(i64, Arc<Value>)> = Vec::new();
            for (k, v_thunk) in args_entries.iter() {
                if let HashableValue::Int(idx) = k {
                    if let Some(Ok(v)) = v_thunk.peek_result() {
                        indexed.push((*idx, Arc::new(v.clone())));
                    }
                }
            }
            indexed.sort_by_key(|(i, _)| *i);
            let args: Vec<Arc<Value>> = indexed.into_iter().map(|(_, v)| v).collect();
            Some((class_name, args))
        }
        _ => None,
    }
}

/// Check whether a TypeValue contains any free type variables (unbound TypeValue.Var).
///
/// Returns `true` if any TypeValue.Var in `tv` is not bound in `subst`.
/// Uses a shallow walk — follows the top-level TypeVar chain and inspects settled
/// payload dicts one level deep (same coverage as InferenceContext::free_vars).
fn has_free_type_vars(tv: &Arc<Value>, subst: &HashMap<String, Arc<Value>>) -> bool {
    has_free_type_vars_inner(tv, subst, &mut std::collections::HashSet::new())
}

fn has_free_type_vars_inner(
    tv: &Arc<Value>,
    subst: &HashMap<String, Arc<Value>>,
    visited: &mut std::collections::HashSet<String>,
) -> bool {
    match tv.as_ref() {
        Value::Variant { ctor, payload, .. } => match ctor.as_ref() {
            TV_VAR => {
                // Extract var name from payload dict.
                let name = match payload {
                    Some(thunk) => match thunk.peek_result() {
                        Some(Ok(Value::Dict { entries, .. })) => {
                            let key = HashableValue::Str(Arc::from(FIELD_NAME));
                            match entries.get(&key).and_then(|t| t.peek_result()) {
                                Some(Ok(Value::String {
                                    source, start, end, ..
                                })) => source[*start..*end].to_string(),
                                _ => return true, // unreadable — treat as free
                            }
                        }
                        _ => return true,
                    },
                    None => return true,
                };
                if visited.contains(&name) {
                    return false; // cycle — treat as bound (avoid infinite recursion)
                }
                match subst.get(&name) {
                    Some(bound) => {
                        visited.insert(name);
                        has_free_type_vars_inner(bound, subst, visited)
                    }
                    None => true, // free variable
                }
            }
            // Leaf/opaque variants have no type variable positions.
            TV_UNKNOWN | TV_NEVER | TV_TOP | TV_REPR | TV_INT_LIT | TV_FLOAT_LIT | TV_STR_LIT
            | TV_OP => false,
            // Structural variants: inspect settled payload dicts.
            _ => {
                if let Some(thunk) = payload {
                    if let Some(Ok(Value::Dict { entries, .. })) = thunk.peek_result() {
                        for (_k, v_thunk) in entries.iter() {
                            if let Some(Ok(v)) = v_thunk.peek_result() {
                                match v {
                                    Value::Variant { .. } => {
                                        let arc = Arc::new(v.clone());
                                        if has_free_type_vars_inner(&arc, subst, visited) {
                                            return true;
                                        }
                                    }
                                    Value::Dict {
                                        entries: inner_entries,
                                        ..
                                    } => {
                                        for (_ik, iv_thunk) in inner_entries.iter() {
                                            if let Some(Ok(iv)) = iv_thunk.peek_result() {
                                                if matches!(iv, Value::Variant { .. }) {
                                                    let arc = Arc::new(iv.clone());
                                                    if has_free_type_vars_inner(
                                                        &arc, subst, visited,
                                                    ) {
                                                        return true;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                false
            }
        },
        _ => false,
    }
}

/// Find the instance whose source-position type patterns match `source_types`,
/// and return the full field map of that instance.
///
/// The `source_positions` are the FD-determining parameter positions. We look
/// for an instance whose `instance_type` record field at each source position
/// structurally matches the corresponding ground `source_types[i]`.
///
/// On success, returns the `HashMap<i64, Arc<Value>>` of all fields from the
/// matched instance's `instance_type` record. Callers select target positions
/// by key (e.g., `field_map.get(&(target_pos as i64))`).
fn lookup_instance_for_fd(
    class_name: &str,
    source_positions: &[usize],
    source_types: &[Arc<Value>],
    env: &std::sync::Arc<std::sync::RwLock<crate::env::Env>>,
) -> Option<HashMap<i64, Arc<Value>>> {
    let env_guard = env.read().unwrap();
    let instances = env_guard.all_instances();
    drop(env_guard);

    for (_, inst) in &instances {
        if inst.class_name != class_name {
            continue;
        }
        // inst.instance_type is a TypeValue.Record { fields: { 0: T0, 1: T1, ... }, ... }
        // for multi-parameter classes, or the direct TypeValue for single-param classes.
        let inst_type = &inst.instance_type;
        // For MPTC instances, extract fields from the Record payload.
        let field_map = extract_instance_record_fields(inst_type);
        // Match source positions against the instance's field types.
        let mut all_match = true;
        for (i, &pos) in source_positions.iter().enumerate() {
            let inst_arg = match field_map.get(&(pos as i64)) {
                Some(t) => t.clone(),
                None => {
                    all_match = false;
                    break;
                }
            };
            // Use structural TypeValue matching: TypeValue.Var in the instance pattern
            // matches any target (polymorphic wildcard). Concrete types must match exactly.
            if !typevalue_matches_fd(&inst_arg, &source_types[i]) {
                all_match = false;
                break;
            }
        }
        if all_match {
            return Some(field_map);
        }
    }
    None
}

/// Extract integer-keyed fields from a TypeValue.Record payload.
/// Returns a HashMap from integer position → TypeValue.
/// For single-param instances where the instance_type is NOT a Record,
/// returns a single-entry map { 0: instance_type }.
fn extract_instance_record_fields(tv: &Arc<Value>) -> HashMap<i64, Arc<Value>> {
    let mut result = HashMap::new();
    match tv.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => {
            if let Some(Ok(Value::Dict { entries, .. })) = thunk.peek_result() {
                let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                if let Some(fields_thunk) = entries.get(&fields_key) {
                    if let Some(Ok(Value::Dict {
                        entries: f_entries, ..
                    })) = fields_thunk.peek_result()
                    {
                        for (k, v_thunk) in f_entries.iter() {
                            if let HashableValue::Int(idx) = k {
                                if let Some(Ok(v)) = v_thunk.peek_result() {
                                    result.insert(*idx, Arc::new(v.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {
            // Single-parameter instance: the instance_type IS the type directly.
            result.insert(0, Arc::clone(tv));
        }
    }
    result
}

/// Structural matching for FD instance lookup: does `pattern` match `target`?
///
/// TypeValue.Var in `pattern` is a wildcard (matches any target).
/// All other constructors must match by ctor tag. Repr-typed patterns must
/// match the target's repr string exactly.
fn typevalue_matches_fd(pattern: &Arc<Value>, target: &Arc<Value>) -> bool {
    match (pattern.as_ref(), target.as_ref()) {
        // Var in pattern = polymorphic wildcard — matches anything.
        (Value::Variant { ctor, .. }, _) if ctor.as_ref() == TV_VAR => true,
        // Both unit variants with same ctor — equal.
        (
            Value::Variant {
                ctor: ca,
                payload: None,
                ..
            },
            Value::Variant {
                ctor: cb,
                payload: None,
                ..
            },
        ) => ca.as_ref() == cb.as_ref(),
        // TypeValue.Repr: match by repr string.
        (
            Value::Variant {
                ctor: ca,
                payload: Some(pa),
                ..
            },
            Value::Variant {
                ctor: cb,
                payload: Some(pb),
                ..
            },
        ) if ca.as_ref() == TV_REPR && cb.as_ref() == TV_REPR => {
            let repr_str = |thunk: &Arc<Thunk>| -> Option<String> {
                if let Some(Ok(Value::Dict { entries, .. })) = thunk.peek_result() {
                    let key = HashableValue::Str(Arc::from(FIELD_REPR));
                    if let Some(Ok(Value::String {
                        source, start, end, ..
                    })) = entries.get(&key).and_then(|t| t.peek_result())
                    {
                        return Some(source[*start..*end].to_string());
                    }
                }
                None
            };
            repr_str(pa) == repr_str(pb)
        }
        // Same ctor with payload: match by ctor tag only. This is correct for all
        // builtin TypeValue constructors (Int, Float, Str, Bool, etc.) where the
        // ctor tag alone determines the type. Structural payload comparison is not
        // performed — parametric types whose ctor tags match but payloads differ
        // (e.g. Seq[Int] vs Seq[Str]) are treated as matching here. Callers that
        // need structural comparison must use full TypeValue unification.
        (Value::Variant { ctor: ca, .. }, Value::Variant { ctor: cb, .. }) => {
            ca.as_ref() == cb.as_ref()
        }
        _ => false,
    }
}

/// Functional dependency improvement pass.
///
/// For each constraint in `constraints`, if the constraint's class has FD rules and
/// the source-position types are all ground (no free TypeVars), compute the target
/// type and return `(target_arg, computed_ty)` pairs for the caller to unify.
///
/// Returns an empty Vec when no improvement is possible or `state.fd_depth >= 32`.
/// The caller runs a fixpoint loop: call `try_fd_improvement`, unify all returned
/// pairs, then repeat until the returned Vec is empty.
///
/// The returned pairs are `(target_arg TypeValue, computed_ty TypeValue)` where
/// `target_arg` is a free TypeVar and `computed_ty` is a ground type. The caller
/// calls `unify(target_arg, computed_ty, ...)` for each pair.
pub(crate) async fn try_fd_improvement(
    constraints: &[Arc<Value>],
    ctx: &crate::type_infer::InferenceContext,
    state_env: &std::sync::Arc<std::sync::RwLock<crate::env::Env>>,
    fd_depth: &mut u32,
    eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
    type_stage_fns: &std::collections::HashMap<String, std::sync::Arc<crate::value::Thunk>>,
    diagnostics: &mut Vec<crate::error::Diagnostic>,
) -> Vec<(Arc<Value>, Arc<Value>)> {
    if *fd_depth >= 32 {
        return vec![];
    }
    *fd_depth += 1;
    let mut pairs = Vec::new();

    for constraint in constraints {
        let (class_name, args) = match extract_fd_constraint_parts(constraint) {
            Some(v) => v,
            None => continue,
        };
        // Look up the class declaration.
        let class_decl = {
            let env_guard = state_env.read().unwrap();
            env_guard.get_class(&class_name)
        };
        let class_decl = match class_decl {
            Some(c) => c,
            None => continue,
        };

        for (source_positions, target_positions) in &class_decl.determines {
            // Collect ground source types (apply substitution, check ground).
            let mut source_types: Vec<Arc<Value>> = Vec::new();
            let mut all_ground = true;
            for &pos in source_positions {
                let arg = match args.get(pos) {
                    Some(a) => ctx.apply_subst(a),
                    None => {
                        all_ground = false;
                        break;
                    }
                };
                if has_free_type_vars(&arg, &ctx.subst) {
                    all_ground = false;
                    break;
                }
                source_types.push(arg);
            }
            if !all_ground {
                continue;
            }

            // Compute the determined type via instance lookup or resolver.
            //
            // Resolver-based classes (e.g. Indexable with FieldType) call a type-stage
            // function that takes the ground source types as positional arguments and
            // returns the determined TypeValue directly. Instance-based classes (e.g.
            // Addable, Multipliable) scan registered instances for a structural match.
            if let Some(ref resolver_name) = class_decl.resolver {
                // Resolver-based FD improvement: the caller (run_fd_improvement_fixpoint) passes
                // state.eval_ctx so that the resolver function can be invoked when an EvalContext
                // is available (e.g., during builtin-typecheck in an active eval pipeline).
                //
                // When eval_ctx is None (unit tests, bootstrap, or plain typecheck_file paths),
                // we skip the resolver call — instance-based FD improvement still fires for
                // classes that have both a resolver and instances registered.
                let eval_ctx = match eval_ctx.as_ref() {
                    Some(ec) => ec,
                    None => continue,
                };

                // Look up the resolver thunk from the type-stage function registry.
                // Resolver functions (e.g., FieldType, ElementType) are parameterized type
                // constructors stored as Arc<Thunk> in type_stage_fns, registered when
                // their [class ... resolver: FnName] declaration is processed.
                let resolver_thunk = match type_stage_fns.get(resolver_name.as_str()) {
                    Some(t) => Arc::clone(t),
                    None => continue, // Resolver not registered in type-stage — skip this FD.
                };

                // Convert source types to TypeNode arguments.
                // TypeValues ARE TypeNodes after the S-1003 migration — pass them directly.
                let tn_args: Vec<_> = source_types
                    .iter()
                    .filter_map(|tv| typevalue_to_typenode(tv))
                    .collect();

                // If any source type failed to convert, skip (resolver cannot be called with incomplete args).
                if tn_args.len() != source_types.len() {
                    continue;
                }

                // Call the resolver function. The result is already a TypeValue (evaluate_resolver_with_thunk
                // runs typenode_value_to_type internally and returns Arc<Value> TypeValue).
                let computed_ty = match crate::type_normalize::evaluate_resolver_with_thunk(
                    resolver_thunk,
                    &tn_args,
                    eval_ctx,
                )
                .await
                {
                    Ok(Some(tv)) => tv,
                    Ok(None) => continue, // Resolver not applicable — skip.
                    Err(eval_err) => {
                        diagnostics.push(crate::error::Diagnostic::error(
                            "type-error",
                            format!(
                                "resolver `{}` failed during FD improvement: {}",
                                resolver_name, eval_err
                            ),
                            boot_span(),
                        ));
                        continue;
                    }
                };

                // Skip if the computed type is uninformative (Unknown or free TypeVar).
                match computed_ty.as_ref() {
                    Value::Variant { ctor, .. }
                        if ctor.as_ref() == TV_UNKNOWN || ctor.as_ref() == TV_VAR =>
                    {
                        continue;
                    }
                    _ => {}
                }

                // For each target position with a free TypeVar arg, emit an improvement pair.
                for &target_pos in target_positions {
                    let target_arg = match args.get(target_pos) {
                        Some(a) => ctx.apply_subst(a),
                        None => continue,
                    };
                    if !has_free_type_vars(&target_arg, &ctx.subst) {
                        continue; // Already ground — nothing to improve.
                    }
                    pairs.push((target_arg, Arc::clone(&computed_ty)));
                }

                continue;
            }

            let instance_field_map = lookup_instance_for_fd(
                class_name.as_str(),
                source_positions,
                &source_types,
                state_env,
            );

            let instance_field_map = match instance_field_map {
                Some(f) => f,
                None => continue,
            };

            // For each target position, if the constraint arg is a free TypeVar,
            // generate a (target_arg, computed_type) pair.
            for &target_pos in target_positions {
                let target_arg = match args.get(target_pos) {
                    Some(a) => ctx.apply_subst(a),
                    None => continue,
                };
                if !has_free_type_vars(&target_arg, &ctx.subst) {
                    continue; // Already ground — nothing to improve.
                }
                // Compute the type for this target position from the matched instance's field map.
                let computed_ty = match instance_field_map.get(&(target_pos as i64)) {
                    Some(t) => Arc::clone(t),
                    None => continue,
                };
                // Skip if the computed type is itself Unknown or a TypeVar (uninformative).
                match computed_ty.as_ref() {
                    Value::Variant { ctor, .. }
                        if ctor.as_ref() == TV_UNKNOWN || ctor.as_ref() == TV_VAR =>
                    {
                        continue;
                    }
                    _ => {}
                }
                pairs.push((target_arg, computed_ty));
            }
        }
    }

    *fd_depth -= 1;
    pairs
}

/// Convert a TypeValue (Arc<Value>) to a TypeNode (Arc<Value>).
///
/// TypeValues are the internal inference representation; TypeNodes are the user-facing
/// AST-level type representation declared in `stdlib/builtin_core.llt`. Resolver-based
/// FD improvement functions (e.g. FieldType, ElementType) expect TypeNode arguments.
///
/// Conversions:
/// - TypeValue.Repr { repr: "Value::Int" }    → Variant("TypeNode.Int", None)
/// - TypeValue.Repr { repr: "Value::Float" }  → Variant("TypeNode.Float", None)
/// - TypeValue.Repr { repr: "Value::String" } → Variant("TypeNode.String", None)
/// - TypeValue.Repr { repr: "Value::Bytes" }  → Variant("TypeNode.Bytes", None)
/// - TypeValue.Var { name }                   → Variant("TypeNode.TypeVar", { name })
/// - TypeValue.Unknown                        → Variant("TypeNode.Unknown", None)
/// - TypeValue.Top                            → Variant("TypeNode.Top", None)
/// - TypeValue.Never                          → Variant("TypeNode.Never", None)
///
/// Returns `None` for TypeValues that have no direct TypeNode equivalent (compound types
/// like Fn, Record, Union, etc. are not yet converted — extend as needed).
pub(crate) fn typevalue_to_typenode(tv: &Arc<Value>) -> Option<Arc<Value>> {
    use crate::type_infer::typevalue_ctor;

    match typevalue_ctor(tv) {
        Some(TV_REPR) => {
            let repr_field = crate::type_infer::typevalue_payload_field(tv, FIELD_REPR)?;
            let repr_str = match repr_field.as_ref() {
                Value::String {
                    source, start, end, ..
                } => source[*start..*end].to_string(),
                _ => return None,
            };
            let tn_ctor = match repr_str.as_str() {
                REPR_INT => TN_INT,
                REPR_FLOAT => TN_FLOAT,
                REPR_STRING => TN_STRING,
                REPR_BYTES => TN_BYTES,
                // REPR_DICT not mapped: TypeNode.Dict requires structural payload (fields, open)
                // that TypeValue.Repr lacks. Use TypeNode.Dict construction explicitly.
                REPR_DECIMAL => TN_DECIMAL,
                REPR_BIGINT => TN_BIG_INT,
                REPR_DIR_CAP => TN_DIR_CAP,
                REPR_NET_CAP => TN_NET_CAP,
                REPR_FILE => TN_FILE,
                REPR_TASK => TN_TASK,
                REPR_CHANNEL => TN_CHANNEL,
                REPR_CONTEXT => TN_CONTEXT,
                REPR_REACTIVE_CELL => TN_REACTIVE_CELL,
                REPR_CLOCK_CAP => TN_CLOCK_CAP,
                REPR_TIMEZONE => TN_TIMEZONE,
                REPR_TIMESTAMP => TN_TIMESTAMP,
                REPR_DURATION => TN_DURATION,
                REPR_PROXY => TN_PROXY,
                REPR_QUIC_SESSION => TN_QUIC_SESSION,
                REPR_QUIC_DATAGRAM_HANDLE => TN_QUIC_DATAGRAM_HANDLE,
                REPR_HTTP2_SESSION => TN_HTTP2_SESSION,
                REPR_HTTP3_SESSION => TN_HTTP3_SESSION,
                REPR_URI => TN_URI,
                REPR_PROGRAM => TN_PROGRAM,
                REPR_DOCUMENT => TN_DOCUMENT,
                REPR_CORE_DOCUMENT => TN_CORE_DOCUMENT,
                REPR_TYPE_CONTEXT => TN_TYPE_CONTEXT,
                _ => return None,
            };
            Some(Arc::new(Value::Variant {
                ctor: Arc::from(tn_ctor),
                payload: None,
                type_val: unknown_type_val(),
                type_decl_id: 0,
            }))
        }
        Some(TV_VAR) => {
            let name = crate::type_infer::typevalue_var_name(tv)?;
            let payload = single_field_dict(TN_FIELD_NAME, string_value(&name));
            Some(Arc::new(make_variant(TN_TYPE_VAR, Some(payload))))
        }
        Some(TV_UNKNOWN) => Some(Arc::new(Value::Variant {
            ctor: Arc::from(TN_UNKNOWN),
            payload: None,
            type_val: unknown_type_val(),
            type_decl_id: 0,
        })),
        Some(TV_TOP) => Some(Arc::new(Value::Variant {
            ctor: Arc::from(TN_TOP),
            payload: None,
            type_val: unknown_type_val(),
            type_decl_id: 0,
        })),
        Some(TV_NEVER) => Some(Arc::new(Value::Variant {
            ctor: Arc::from(TN_NEVER),
            payload: None,
            type_val: unknown_type_val(),
            type_decl_id: 0,
        })),
        _ => None, // Compound types not yet converted — extend as needed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_typevar_local(name: &str) -> Arc<Value> {
        let payload = single_field_dict("name", string_value(name));
        Arc::new(make_variant(TV_VAR, Some(payload)))
    }

    /// make_constraint_decl produces a ConstraintDecl Variant.
    #[test]
    fn test_make_constraint_decl_ctor() {
        let class_tv = make_type_op("Sortable");
        let arg_tv = make_typevar_local("a");
        let constraint = make_constraint_decl(class_tv, vec![arg_tv]);

        match constraint.as_ref() {
            Value::Variant { ctor, .. } => {
                assert_eq!(ctor.as_ref(), TV_CONSTRAINT_DECL);
            }
            _ => panic!("expected Value::Variant for ConstraintDecl"),
        }
    }
}
