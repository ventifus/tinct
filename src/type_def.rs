//! Core type representations for the LLT type system.
//!
//! **S-1003 migration**: The `Type` Rust enum, `Row`/`RowTail` row structs, and `Kind`
//! enum have all been deleted. Type representations now use `Arc<Value>` (TypeValue)
//! throughout. See doc/06-type-inference.md and doc/whatif/runtime-types.md.
//!
//! Surviving items:
//! - `Variance` — type parameter variance annotation
//! - `TyConDef` — type constructor definition (body: Arc<Value> after T-1986)
//! - `TyConEnv` — type constructor environment
//! - `instance_binding_name` — gensym naming for instance specializations
//!
//! Inference machinery (`InferState`, generalization) lives in `type_infer.rs`.
//! Unification lives in `type_unify.rs`.
//! Type class declarations (`ClassDecl`, `Constraint`) live in `type_class.rs`.
//! BAS subtyping (normalized disjunctive normal form) lives in `bas.rs`.
//! Normalization and Display impls live in `type_normalize.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;

// T-2001: RowTail and Row deleted. Row types now use Arc<Value> TypeValues directly.
//
// Row construction:
//   Closed row (was RowTail::Empty):
//     Value::Variant { ctor: Arc::from("RowTail.Closed"), payload: None, type_val: unknown_type_val() }
//   Open row with variable (was RowTail::Var(name)):
//     Value::Variant { ctor: Arc::from("RowTail.Var"), payload: Some(settled_string(name)), type_val: unknown_type_val() }
//
// TypeValue.Record { fields: Dict, tail: RowTail.* } represents the full row type.

// T-1995: Kind enum deleted. Kind information is now carried as Arc<Value> TypeValues.
// Kind::Type      → TypeValue.Op { name: "Type" }   (proper type kind *)
// Kind::Operator  → TypeValue.Op { name: "Operator" }  (type constructor kind * → *)
// Kind::Arrow(k1,k2) → TypeValue.Fn { params: [k1], return: k2 }  (higher-kinded)
// Kind::Label     → TypeValue.Op { name: "Label" }  (record field label kind)
// InferState.kind_env: HashMap<String, Kind> → HashMap<String, Arc<Value>>

// T-1986: pub enum Type deleted. All type representations now use Arc<Value> TypeValues.
// TypeValue constructor tags and mappings are documented in doc/06-type-inference.md.
// See also: doc/whatif/runtime-types.md for the complete TypeValue specification.
//
// Key mapping (Type variant → TypeValue ctor tag):
//   Type::Int           → TypeValue.Repr { repr: "Value::Int" }
//   Type::Float         → TypeValue.Repr { repr: "Value::Float" }
//   Type::Str           → TypeValue.Repr { repr: "Value::String" }
//   Type::Bool          → TypeValue.Repr { repr: "Value::Bool" }
//   Type::Bytes         → TypeValue.Repr { repr: "Value::Bytes" }
//   Type::Unknown       → TypeValue.Unknown
//   Type::Any           → TypeValue.Top
//   Type::Never         → TypeValue.Never
//   Type::Var(n,_)      → TypeValue.Var { name: n }
//   Type::Function{..}  → TypeValue.Fn { params: Dict, return: TypeValue }
//   Type::Dict(Row)     → TypeValue.Record { fields: Dict, tail: RowTail.* }
//   Type::Union(v)      → TypeValue.Union { members: Dict }
//   Type::Intersection  → TypeValue.Inter { members: Dict }
//   Type::Negation(t)   → TypeValue.Neg { of: TypeValue }
//   Type::Recursive{..} → TypeValue.Recursive { body: TypeValue } (T-1997 de Bruijn)
//   RecursiveRef(n)     → TypeValue.RecursiveRef { depth: n }
//   Type::TyCon(n)      → TypeValue.Op { name: n }
//   Type::App(f,a)      → TypeValue.App { op: TypeValue, arg: TypeValue }
//   Type::IntLiteral(n) → TypeValue.IntLit { n: n }
//   Type::StringLiteral → TypeValue.StrLit { s: s }
//   RowTail::Empty      → RowTail.Closed
//   RowTail::Var(n)     → RowTail.Var { name: n }

/// Variance annotation for type parameters.
/// Used in TyConDef to specify how type arguments vary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variance {
    /// Type parameter appears only in positive positions (e.g., return types)
    Covariant,
    /// Type parameter appears only in negative positions (e.g., function arguments)
    Contravariant,
    /// Type parameter appears in both positive and negative positions
    Invariant,
    /// Type parameter does not appear in the type body (phantom type)
    Phantom,
}

/// Type constructor definition.
/// Stores variance information and constructor tags for user-defined types.
///
/// T-1986: `body` field changed from `Type` to `Arc<crate::value::Value>` (TypeValue).
/// The body is a TypeValue representing the resolved type for this constructor.
/// During bootstrap (before repr wiring), `body` is `crate::value::unknown_type_val()`.
#[derive(Debug, Clone)]
pub struct TyConDef {
    /// Type parameter names (e.g., ["a", "k", "v"]). Empty for zero-parameter types.
    pub params: Vec<String>,

    /// Type body as a TypeValue (Arc<Value>). For structural aliases, this is the expanded
    /// TypeValue; for nominal ADTs, this is typically a TypeValue.Union of NominalVariants.
    /// During bootstrap, holds `unknown_type_val()` as a placeholder.
    pub body: Arc<crate::value::Value>,

    /// Class constraints on type parameters, populated when params carry `@ClassName` annotations.
    /// Empty for unconstrained aliases. After S-1003: constraints are Arc<Value> ConstraintDecls.
    pub constraints: Vec<Arc<crate::value::Value>>,

    /// Variance for each type parameter
    pub variance: Vec<Variance>,
    /// Constructors as (tag, arity) pairs
    pub constructors: Vec<(String, usize)>,
    /// Optional builtin type discriminant (e.g., "Seq", "Map")
    pub builtin_type: Option<String>,
    /// Annotation dict attached to the type constructor declaration via `@[...]` syntax.
    ///
    /// `None` until T-1122 populates it from `@[...]` annotations on `[type ...]`
    /// declarations via eval_type_stage_expr. At type-check time, `annotation-of` on a
    /// TyConDef reference reads this field.
    ///
    /// Uses `IndexMap` to preserve annotation key insertion order for user-facing output.
    pub annotation: Option<IndexMap<String, crate::value::Value>>,
    /// Field-level annotation dicts for record-type constructors, keyed by field name.
    ///
    /// Maps each annotated field name to its `@[...]` annotation dict (e.g.
    /// `host@[required: true  doc: "hostname"]: String` → `{"host": {"required": true, "doc": "hostname"}}`).
    /// Used by the TypeNode protocol to derive `children`/`map-children` roles from `@Child`
    /// field annotations. Populated by lower.rs::infer_child_role_from_type_expr
    /// from @Child field annotations.
    ///
    /// Both outer and inner maps use `IndexMap` to preserve annotation key insertion order.
    pub field_annotations: IndexMap<String, IndexMap<String, crate::value::Value>>,

    /// Compile-time constants for each variant constructor (T-1357/T-1358).
    ///
    /// Maps qualified constructor tag → (constant name → constant value).
    /// Populated from `name: literal` entries in variant declarations:
    ///   `[NoError rcode: 0 description: "No Error"]` → `{ "DnsRcode.NoError": { "rcode": 0, "description": "No Error" } }`
    ///
    /// Empty for types without constants. Constants are stored as `Value` literals
    /// (Int, U64, Float, String) — complex expressions are not valid constant entries.
    ///
    /// Forward lookup: when `.field` is accessed on a `Variant { tag, payload: None }` or
    /// after payload access fails, the evaluator looks up `tag` in this map and returns the
    /// constant for that field. This enables `some-rcode.rcode` without a match expression.
    pub constructor_constants: IndexMap<String, IndexMap<String, crate::value::Value>>,

    /// Source span of the `[type ...]` declaration that defined this type constructor.
    /// Used in error messages to show where conflicting types were defined.
    pub definition_span: Option<Span>,
}

impl PartialEq for TyConDef {
    fn eq(&self, other: &Self) -> bool {
        // Compare by params, variance, constructors, builtin_type — not body (Arc<Value>
        // comparison would need deep equality which is expensive and not meaningful for
        // type identity). Two TyConDefs are equal if they describe the same nominal structure.
        self.params == other.params
            && self.variance == other.variance
            && self.constructors == other.constructors
            && self.builtin_type == other.builtin_type
    }
}

impl TyConDef {
    pub fn arity(&self) -> usize {
        self.params.len()
    }

    /// Create a new TyConDef for a zero-arity nominal type with its resolved body.
    ///
    /// Convenience constructor for registration and testing (T-1112). The `body` is the
    /// resolved TypeValue for the type (e.g., a TypeValue.Union of NominalVariants).
    pub fn new_with_body(_name: impl Into<String>, body: Arc<crate::value::Value>) -> Self {
        Self {
            params: vec![],
            body,
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: IndexMap::new(),
            constructor_constants: IndexMap::new(),
            definition_span: None,
        }
    }

    /// Create a new TyConDef for a parameterized type constructor with the given arity.
    ///
    /// Convenience constructor for registration and testing (T-1112). The body is set to
    /// `unknown_type_val()` (opaque until instantiated with type arguments).
    pub fn new_parameterized(arity: usize) -> Self {
        Self {
            params: (0..arity).map(|i| format!("a{i}")).collect(),
            body: crate::value::unknown_type_val(),
            constraints: vec![],
            variance: vec![Variance::Invariant; arity],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: IndexMap::new(),
            constructor_constants: IndexMap::new(),
            definition_span: None,
        }
    }
}

/// Type constructor environment mapping type constructor names to their definitions.
///
/// Values are `Arc<TyConDef>` so that distinct scope insertions of the same name produce
/// distinct Arcs. `Arc::ptr_eq` in UNIFY-TYCON can then detect shadowing: if two TyCon("Foo")
/// types came from different `[type Foo ...]` declarations in different scopes, their Arcs
/// will differ even though the name string is equal.
pub type TyConEnv = HashMap<String, Arc<TyConDef>>;

// InferenceContext is defined in type_infer.rs (canonical location).
// It was duplicated here during the S-1003 migration; the duplicate has been removed.
// Import it via `crate::type_infer::InferenceContext` or `crate::types::InferenceContext`.

/// Generate the gensym'd binding name for a compile-time instance specialization.
///
/// When `[instance Equatable [let k@Int]: [=: impl-fn]]` is compiled, this produces
/// the name `ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽` which is stored as a regular dict binding
/// in the evaluation environment. Call-site rewriting in lower.rs looks up this name
/// to produce a direct binding reference rather than going through the method dispatch
/// sentinel machinery.
///
/// All component characters are valid tinct identifier chars (not in the denylist).
/// `∷` (U+2237, PROPORTION) distinguishes class from method without being DotAccess.
/// `⧼`/`⧽` (U+29FC/U+29FD) are the gensym delimiters used elsewhere in tinct.
/// `⟨`/`⟩` (U+27E8/U+27E9) delimit type arguments.
pub fn instance_binding_name(class: &str, method: &str, type_args: &[&str]) -> String {
    let args = type_args.join(",");
    if args.is_empty() {
        format!("ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⧽")
    } else {
        format!("ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽")
    }
}

// T-1995: check_kind_wellformed deleted along with Kind enum.
// Kind checking is now done at the TypeValue level (Arc<Value>) by other machinery.

#[cfg(test)]
mod tests {
    use super::*;

    // T-1995: Kind tests deleted along with Kind enum.
    // T-1986: Type-based subtyping tests deleted along with Type enum.
    // T-2001: Row/RowTail tests deleted along with those types.

    #[test]
    fn test_instance_binding_name_no_args() {
        assert_eq!(
            instance_binding_name("Equatable", "=", &[]),
            "ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⧽"
        );
    }

    #[test]
    fn test_instance_binding_name_with_args() {
        assert_eq!(
            instance_binding_name("Addable", "+", &["Int", "Int"]),
            "ɪɴꜱᴛᴀɴᴄᴇ⧼Addable∷+⟨Int,Int⟩⧽"
        );
    }

    #[test]
    fn test_tycondef_new_parameterized_sets_invariant_variance() {
        let def = TyConDef::new_parameterized(2);
        assert_eq!(def.params.len(), 2);
        assert_eq!(def.variance, vec![Variance::Invariant, Variance::Invariant]);
        assert!(def.constructors.is_empty());
    }

    #[test]
    fn test_tycondef_arity() {
        let def = TyConDef::new_parameterized(3);
        assert_eq!(def.arity(), 3);
    }

    #[test]
    fn test_tycondef_partial_eq_by_structure() {
        let def1 = TyConDef::new_parameterized(2);
        let def2 = TyConDef::new_parameterized(2);
        // Same structure → equal (PartialEq ignores body Arc<Value>)
        assert_eq!(def1, def2);
    }

    #[test]
    fn test_variance_partial_eq() {
        assert_eq!(Variance::Covariant, Variance::Covariant);
        assert_ne!(Variance::Covariant, Variance::Contravariant);
    }
}
