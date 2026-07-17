//! Core type representations for the LLT type system.
//!
//! This module contains the `Type` enum, `Row` struct for record types, kind definitions,
//! and purely structural operations on types (subtyping, consistency, variable collection).
//!
//! Inference machinery (`InferState`, generalization) lives in `type_infer.rs`.
//! Unification lives in `type_unify.rs`.
//! Type class declarations (`ClassDecl`, `Constraint`) live in `type_class.rs`.
//! Normalization and Display impls live in `type_normalize.rs`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::type_errors::{GenericTypeError, TypeErrorTyped};
use crate::types::TypeError;

/// Tail of a row type — either closed (no additional fields) or uniform (additional fields
/// all have the same value type, optionally also constrained to a specific key type).
///
/// `Empty` = closed record: `{f1: T1, f2: T2}` — no other fields allowed.
/// `Uniform { key: None, value: V }` = open record with uniform value type: `{f1: T1, _ : V}`
///   — any additional field must have value type V.
/// `Uniform { key: Some(K), value: V }` = typed-key column constraint: `{f1: T1, _@K : V}`
///   — additional fields' keys must have type K, values type V.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowTail {
    Empty,
    Uniform {
        key: Option<Box<Type>>,
        value: Box<Type>,
    },
}

/// Row representation for record types.
///
/// `fields` uses `IndexMap` to preserve insertion order (source declaration order).
/// Insertion order defines the canonical slot numbering used by `slot-get` for
/// O(1) positional field access. Although row field order is semantically irrelevant at
/// the type level (structural subtyping makes rows unordered), insertion order IS the
/// canonical slot ordering that the type checker writes into `SlotAnnotation` fields on DotAccess nodes
/// and the lowerer reads to emit `Call(slot-get, [Int(slot), target])` vs `Call(field-get, [Str(key), target])`.
///
/// `PartialEq`, `Eq`, and `Hash` are order-independent (field set equality, not sequence
/// equality) so that type equality is unaffected by the order fields were added to a row.
///
/// `tail` constrains the non-named portion of the row. `RowTail::Empty` is the default for
/// all current closed-record constructions. `RowTail::Uniform` is produced when parsing
/// `{_ : V}` or `{_@K : V}` annotation syntax (column constraints).
#[derive(Debug, Clone)]
pub struct Row {
    pub fields: IndexMap<String, Type>, // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,
}

impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        self.tail == other.tail
            && self.fields.len() == other.fields.len()
            && self
                .fields
                .iter()
                .all(|(k, v)| other.fields.get(k) == Some(v))
    }
}

impl Eq for Row {}

impl std::hash::Hash for Row {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let mut pairs: Vec<(&String, &Type)> = self.fields.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in pairs {
            k.hash(state);
            v.hash(state);
        }
        self.tail.hash(state);
    }
}

/// Kind for higher-kinded types (Jones 1993)
/// Kinds classify types: * for proper types, Operator for type constructors (* → *)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    /// * — kind of proper types (Int, Str, [name: Str], etc.)
    Type,
    /// Operator — kind of type constructors (* → *).
    /// Used for type constructor variables like `m` in `Monad m`.
    Operator,
    /// k₁ → k₂ — kind of multi-argument type constructors.
    /// Used for builtin type constructors like Map (* → * → *).
    Arrow(Box<Kind>, Box<Kind>),
    /// Label — kind of type-level string labels used for record field names.
    /// Used for label TypeVars in `HasField` constraints (e.g., `key@"k"`).
    Label,
}

impl Kind {
    pub fn arity(&self) -> usize {
        match self {
            Kind::Type | Kind::Label => 0,
            Kind::Operator => 1,
            Kind::Arrow(_, ret) => 1 + ret.arity(),
        }
    }

    pub fn is_operator(&self) -> bool {
        matches!(self, Kind::Operator | Kind::Arrow(..))
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Type => write!(f, "*"),
            Kind::Operator => write!(f, "* → *"),
            Kind::Arrow(a, b) => write!(f, "{} → {}", a, b),
            Kind::Label => write!(f, "Label"),
        }
    }
}

/// Label for record field names in HasField constraints.
/// Used in `HasField { label: Label, dict_var: String, field_var: String }`.
/// Provides compile-time structural enforcement that the label position is always
/// a string literal or a label TypeVar name, never an arbitrary Type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Label {
    /// Concrete label — a known field name like "host" or "port"
    Concrete(String),
    /// Label variable — a TypeVar name referencing a Kind::Label variable in kind_env
    Var(String),
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Label::Concrete(s) => write!(f, "\"{}\"", s),
            Label::Var(name) => write!(f, "{}", name),
        }
    }
}

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
#[derive(Debug, Clone, PartialEq)]
pub struct TyConDef {
    /// Type parameter names (e.g., ["a", "k", "v"]). Empty for zero-parameter types.
    pub params: Vec<String>,

    /// Type body. For structural aliases, this is the expanded type; for nominal ADTs,
    /// this is typically a Union of NominalVariants.
    pub body: Type,

    /// Class constraints on type parameters, populated when params carry `@ClassName` annotations.
    /// Empty for unconstrained aliases.
    pub constraints: Vec<crate::type_class::Constraint>,

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
    /// field annotations. Empty until T-1122 populates it.
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
    pub definition_span: Option<crate::ast::Span>,
}

impl TyConDef {
    pub fn arity(&self) -> usize {
        self.params.len()
    }

    /// Create a new TyConDef for a zero-arity nominal type with its resolved body.
    ///
    /// Convenience constructor for registration and testing (T-1112). The `body` is the
    /// resolved union of NominalVariants for zero-arity ADTs (e.g., `Color` → `Union([Red, Green, Blue])`).
    pub fn new_with_body(_name: impl Into<String>, body: Type) -> Self {
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
    /// `Type::Unknown` (opaque until instantiated with type arguments).
    pub fn new_parameterized(name: impl Into<String>, arity: usize) -> Self {
        let _name = name.into(); // name is unused in main tree (TyCon carries name in Type::TyCon(String))
        Self {
            params: (0..arity).map(|i| format!("a{i}")).collect(),
            body: Type::Unknown,
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
/// will differ even though the name string is equal. (B-343)
pub type TyConEnv = HashMap<String, Arc<TyConDef>>;

#[derive(Debug, Clone)]
pub enum Type {
    Int,
    IntLiteral(i64),
    Float,
    Str,
    StringLiteral(String),
    Bytes,
    Dict(Row),
    Function {
        params: Vec<(Option<String>, Type)>, // (param_name, param_type) — None = positional-only
        ret: Box<Type>,
        variadic: bool,
        /// Number of parameters that must be supplied — params without a `default:` annotation.
        /// Callers may omit the trailing `params.len() - required_count` parameters.
        /// For all builtin functions, `required_count == params.len()` (no optional params).
        /// Excluded from PartialEq and Hash so two function types with the same structure but
        /// different optionality still compare equal (structural type identity is unchanged).
        required_count: usize,
    },
    Proxy,
    #[allow(clippy::enum_variant_names)]
    /// Type variable for parametric polymorphism.
    /// The u32 is the creation-time level; InferState.levels[name] holds the current
    /// (possibly lowered) level (Kiselyov 2013). PartialEq ignores the level field
    /// because type variables with the same name are identical regardless of level.
    TypeVar(String, u32),
    /// Unknown type — the gradual typing "?" type. Represents "I don't know the type"
    /// (unannotated params, inference defaults, builtin returns that can't be precisely typed).
    /// Related to other types via CONSISTENCY (~), not subtyping (<:).
    /// Consistency is symmetric but NOT transitive: Int ~ Unknown, Unknown ~ Str, but NOT Int ~ Str.
    /// This prevents the lattice collapse that Any-as-top-and-bottom caused.
    /// See Siek & Taha (2006), Garcia et al. (2016) AGT framework.
    Unknown,
    /// Any type — ⊤, the true supertype of everything. Represents "any type is allowed here"
    /// (TypeAssert upper bound, explicit "accept anything" positions).
    /// All types τ satisfy τ <: Any.
    Any,
    /// Sentinel for failed sub-expression inference. Prevents cascade errors: when a
    /// sub-expression fails type inference, its result is `Error` rather than propagating
    /// the failure to parent expressions. `unify(Error, T)` is a no-op for all T (silent
    /// absorption), so parent expressions can continue inference without spurious downstream
    /// errors. `is_subtype(Error, _)` returns false; Error is not a subtype of anything.
    ///
    /// The payload carries the errors that caused this `Error` node. An empty `Vec` is
    /// FORBIDDEN — always use `Type::error_note(msg)` or `Type::error_with(errs)`.
    /// Every Error must carry causal context; empty payloads make blame impossible.
    Error(Arc<Vec<TypeErrorTyped>>),
    /// Directory capability — wraps cap_std::fs::Dir. Injected via CLI --cap-fs or
    /// runtime env (cwd, libdir). Represents authority to access a specific directory tree.
    DirCap,
    /// Network capability — wraps host allowlist. Injected via CLI --cap-net.
    /// Represents authority to connect to specific network hosts.
    NetCap,
    /// URI — uniform resource identifier with scheme. Represents capability-tagged URLs.
    Uri,
    /// UTC timestamp (nanoseconds since Unix epoch) — created by `parse-timestamp` or `now`.
    Timestamp,
    /// Signed duration (nanoseconds) — created by `duration-*` constructors.
    Duration,
    /// Clock capability — authority to read current time. Injected by default as %clock (disable with --no-cap-clock).
    ClockCap,
    /// Timezone — parsed IANA TZ rules from zoneinfo file. Created by `load-tz`.
    Timezone,
    /// QUIC session — multiplexed connection over UDP (RFC 9000). Created by `quic-session`.
    QuicSession,
    /// HTTP/2 session — multiplexed HTTP connection (RFC 9113). Created by `http2-session`.
    Http2Session,
    /// HTTP/3 session — HTTP over QUIC (RFC 9114). Created by `http3-session`.
    Http3Session,
    /// QUIC datagram handle — unreliable message delivery (RFC 9221). Created by `quic-open-datagram`.
    QuicDatagramHandle,
    /// Datagram socket handle — message-oriented I/O (UDP or Unix datagram).
    /// Created by `connect cap Udp host port`. Consumed by `send-datagram` and `recv-datagram`.
    DatagramHandle,
    /// Union type — represents a value that can be one of several types.
    /// Invariant: members are sorted, deduplicated, and flattened (no nested unions).
    /// Single-element unions are unwrapped to the bare type by normalize_union().
    /// Unions appear in explicit annotations, builtin signatures, and inferred types
    /// when a type variable has multiple lower bounds (algebraic subtyping Phase 3c).
    Union(Vec<Type>),
    /// Intersection type — represents a value that satisfies all constituent types.
    /// Invariant: members are sorted, deduplicated, and flattened (no nested intersections).
    /// Single-element intersections are unwrapped to the bare type by normalize_intersection().
    /// Intersections appear in inferred types when a type variable has multiple upper bounds
    /// (algebraic subtyping Phase 3c). Top is the identity (T & Top = T), Never is absorbing
    /// (T & Never = Never).
    Intersection(Vec<Type>),
    /// Negation type — ~A, the complement type. Represents "definitely not A".
    /// Essential for BAS constraint solving (C-Var1/2 rules) and false-branch narrowing.
    /// Example: after a type predicate guard on x fails, x : (Int | Str) & ~Int = Str.
    /// In annotation syntax: @[[without A]].
    Negation(Box<Type>),
    /// Never type — ⊥, the bottom type. Represents "no value can inhabit this type".
    /// The empty intersection. Intersections that simplify to Never (e.g., Int & Str,
    /// #Ok & #Err via S-ClsBot) become Never. In annotation syntax: @Never.
    Never,
    /// Type constructor application — `App(f, a)` represents type constructor `f` applied to type `a`.
    /// Example: `App(TyCon("Map"), Str)` partially applies Map to Str.
    /// Example: `App(App(TyCon("Map"), Str), Int)` for Map[Str, Int] (curried).
    App(Box<Type>, Box<Type>),
    /// Named type constructor — a concrete type constructor like `Map` or `Handle`.
    /// Used as the head of `App` chains: `App(TyCon("Map"), Str)`.
    /// Display: just the name.
    TyCon(String),
    /// Type constructor variable — represents a type constructor like `m` in `Monad m`.
    /// Kind: `Operator` (i.e., `* → *`). Used in typeclass constraints and generic functions.
    Operator(String),
    /// Type-stage function application — represents a pending type-level computation.
    /// Created during constraint generation for FD classes; reduced by normalize().
    /// Example: TypeStageApp { fn_name: "MyResolver", args: vec![Int, Float] } reduces to the type
    /// returned by the `MyResolver` function in the type-stage env.
    #[allow(clippy::enum_variant_names)]
    // Type prefix is intentional for type-level computation
    TypeStageApp {
        fn_name: String,
        args: Vec<Type>,
    },
    /// Nominal variant — a union member that carries its declared constructor name.
    /// Used for nominal variants like `[Some a]`, `[IntLiteral value: Int span: AstSpan]`, and `None`.
    /// The `tycon` is the type constructor name (e.g., "Option", "IntLiteral", "Boolean") and `ctor` is
    /// the variant constructor name (e.g., "Some", "None", "True"). `fields` are the named or positional
    /// payload fields.
    /// Distinct from structural `Record` types — nominal variants are never subtypes of records.
    NominalVariant {
        tycon: String,
        ctor: String,
        fields: Row,
    },
    /// Equirecursive type — `μvar.body`, where `var` is a globally-unique gensym'd binder
    /// name (produced by `gensym_fresh('𝜇', alias_name)`) and `body` is the expanded type
    /// body that may contain `TypeVar(var, 0)` at recursive positions.
    ///
    /// This is the S-860 equirecursive-types-core wrapping: produced by `expand_named` after
    /// alias expansion when the body contains a self-reference (`contains_recvar` check).
    ///
    /// The `var` name serves as the coinductive sigma key in S-Exp + S-Assum subtype checking.
    /// S-861 implemented `is_subtype` with S-Exp + S-Assum for Recursive types.
    /// `expand_named` wiring into the annotation resolver is deferred to S-862.
    ///
    // S-861: equirecursive-checker — is_subtype done; expand_named wiring deferred to S-862
    #[allow(dead_code)]
    Recursive {
        /// Globally unique μ-binder name (e.g., `"𝜇ꜱʏᴍ⧼IntList⧽42"`).
        var: String,
        /// Expanded type body; `TypeVar(var, 0)` marks recursive positions.
        body: Box<Type>,
    },
}

// Manual PartialEq for Type: TypeVar compares name only, level ignored
impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Type::Int, Type::Int) => true,
            (Type::IntLiteral(v1), Type::IntLiteral(v2)) => v1 == v2,
            (Type::Float, Type::Float) => true,
            (Type::Str, Type::Str) => true,
            (Type::StringLiteral(s1), Type::StringLiteral(s2)) => s1 == s2,
            (Type::Bytes, Type::Bytes) => true,
            (Type::Dict(row1), Type::Dict(row2)) => row1 == row2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                    variadic: v1,
                    required_count: _,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                    variadic: v2,
                    required_count: _,
                },
            ) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|((_, t1), (_, t2))| t1 == t2)
                    && r1 == r2
                    && v1 == v2
            }
            (Type::Proxy, Type::Proxy) => true,
            (Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2,
            (Type::Unknown, Type::Unknown) => true,
            (Type::Any, Type::Any) => true,
            (Type::Error(_), Type::Error(_)) => true,
            (Type::DirCap, Type::DirCap) => true,
            (Type::NetCap, Type::NetCap) => true,
            (Type::Uri, Type::Uri) => true,
            (Type::Timestamp, Type::Timestamp) => true,
            (Type::Duration, Type::Duration) => true,
            (Type::ClockCap, Type::ClockCap) => true,
            (Type::Timezone, Type::Timezone) => true,
            (Type::QuicSession, Type::QuicSession) => true,
            (Type::Http2Session, Type::Http2Session) => true,
            (Type::Http3Session, Type::Http3Session) => true,
            (Type::QuicDatagramHandle, Type::QuicDatagramHandle) => true,
            (Type::DatagramHandle, Type::DatagramHandle) => true,
            (Type::Union(members1), Type::Union(members2)) => members1 == members2,
            (Type::Intersection(members1), Type::Intersection(members2)) => members1 == members2,
            (Type::Negation(t1), Type::Negation(t2)) => t1 == t2,
            (Type::Never, Type::Never) => true,
            (Type::App(f1, a1), Type::App(f2, a2)) => f1 == f2 && a1 == a2,
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (Type::Operator(name1), Type::Operator(name2)) => name1 == name2,
            (
                Type::TypeStageApp {
                    fn_name: fn1,
                    args: args1,
                },
                Type::TypeStageApp {
                    fn_name: fn2,
                    args: args2,
                },
            ) => fn1 == fn2 && args1 == args2,
            (
                Type::NominalVariant {
                    tycon: tycon1,
                    ctor: ctor1,
                    fields: fields1,
                },
                Type::NominalVariant {
                    tycon: tycon2,
                    ctor: ctor2,
                    fields: fields2,
                },
            ) => tycon1 == tycon2 && ctor1 == ctor2 && fields1 == fields2,
            // S-861: equirecursive-checker — Recursive equality: same binder name and same body.
            // Globally unique gensym var names mean two Recursive values are equal iff they are the
            // same logical type. Alpha-equivalence (different var names, same body shape) is tested
            // by is_subtype (S-Exp + S-Assum coinductive algorithm, done in S-861), not by PartialEq.
            (Type::Recursive { var: v1, body: b1 }, Type::Recursive { var: v2, body: b2 }) => {
                v1 == v2 && b1 == b2
            }
            _ => false,
        }
    }
}

impl Eq for Type {}

impl std::hash::Hash for Type {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash the discriminant first
        std::mem::discriminant(self).hash(state);
        match self {
            Type::Int
            | Type::Float
            | Type::Str
            | Type::Bytes
            | Type::Proxy
            | Type::Unknown
            | Type::Any
            | Type::DirCap
            | Type::NetCap
            | Type::Uri
            | Type::Timestamp
            | Type::Duration
            | Type::ClockCap
            | Type::Timezone
            | Type::QuicSession
            | Type::Http2Session
            | Type::Http3Session
            | Type::QuicDatagramHandle
            | Type::DatagramHandle
            | Type::Never => {}
            // Error: hash the discriminant only (payload is intentionally not hashed — all
            // Error nodes are equal to each other regardless of their causal payload).
            Type::Error(_) => {}
            Type::IntLiteral(v) => v.hash(state),
            Type::StringLiteral(s) => s.hash(state),
            Type::Dict(row) => {
                // Delegate to Row::hash which is order-independent (sorted by key).
                row.hash(state);
            }
            Type::Function {
                params,
                ret,
                variadic,
                required_count: _,
            } => {
                // Hash parameter types (ignore names for equality).
                // required_count is intentionally excluded to match PartialEq semantics.
                for (_, ty) in params {
                    ty.hash(state);
                }
                ret.hash(state);
                variadic.hash(state);
            }
            Type::TypeVar(name, _) => name.hash(state), // Ignore level
            Type::Union(members) => members.hash(state),
            Type::Intersection(members) => members.hash(state),
            Type::Negation(ty) => ty.hash(state),
            Type::App(f, a) => {
                f.hash(state);
                a.hash(state);
            }
            Type::TyCon(name) => name.hash(state),
            Type::Operator(name) => name.hash(state),
            Type::TypeStageApp { fn_name, args } => {
                fn_name.hash(state);
                args.hash(state);
            }
            Type::NominalVariant {
                tycon,
                ctor,
                fields,
            } => {
                tycon.hash(state);
                ctor.hash(state);
                fields.hash(state);
            }
            // S-860: equirecursive-types-core
            // Hash the binder name (globally unique) and the body.
            // The globally-unique gensym var name is sufficient for identity.
            Type::Recursive { var, body } => {
                var.hash(state);
                body.hash(state);
            }
        }
    }
}

// MAX_SUBTYPE_DEPTH removed: BAS subtyping via RDNF normalization terminates by
// structural induction (no depth limit needed). Recursive types use coinductive
// S-Assum/S-Exp with the sigma set, bounded by MAX_ATOM_SUBTYPE_DEPTH in bas.rs.

// S-861: equirecursive-checker

/// Replace all `TypeVar(var_name, _)` occurrences in `ty` with `replacement`.
///
/// Used by `unfold_once` (and by unification arms in `type_unify.rs`) to substitute
/// the recursive variable throughout the body. Only replaces `TypeVar` nodes whose name
/// matches `var_name` exactly; all other forms are recursed into structurally.
///
/// This function is capture-avoiding for the μ-binder: if `ty` is
/// `Type::Recursive { var, body }` and `var == var_name`, the inner binder shadows
/// the outer, so we do NOT recurse into the body (the inner occurrence of `var_name`
/// is bound by the inner binder, not the one being substituted).
///
/// Under the gensym-uniqueness invariant (μ-binder names are globally unique), inner
/// binders can never actually shadow an outer one. The guard documents this invariant
/// and makes the function semantically correct in the general case.
pub(crate) fn substitute_recvar(ty: &Type, var_name: &str, replacement: &Type) -> Type {
    match ty {
        // S-861: equirecursive-checker — the recursive self-reference is represented as a
        // TypeVar whose name is the globally-unique μ-binder name produced by
        // gensym_fresh. When we find it, substitute with the full Recursive type.
        Type::TypeVar(name, _) if name == var_name => replacement.clone(),

        // Inner Recursive binder shadows: do not substitute into body if the inner var
        // has the same name as what we are substituting (capture avoidance).
        // The binder `var` re-binds `var_name` in this sub-scope, so occurrences of
        // `TypeVar(var_name, _)` inside `body` refer to the inner binder, not the outer one.
        Type::Recursive { var, .. } if var == var_name => ty.clone(),

        // Recursive types with a different binder: recurse into the body.
        Type::Recursive { var, body } => Type::Recursive {
            var: var.clone(),
            body: Box::new(substitute_recvar(body, var_name, replacement)),
        },

        // Compound types: recurse structurally into all sub-terms.
        Type::Dict(row) => Type::Dict(substitute_recvar_row(row, var_name, replacement)),
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, ty)| (name.clone(), substitute_recvar(ty, var_name, replacement)))
                .collect(),
            ret: Box::new(substitute_recvar(ret, var_name, replacement)),
            variadic: *variadic,
            required_count: *required_count,
        },
        Type::Union(members) => Type::Union(
            members
                .iter()
                .map(|m| substitute_recvar(m, var_name, replacement))
                .collect(),
        ),
        Type::Intersection(members) => Type::Intersection(
            members
                .iter()
                .map(|m| substitute_recvar(m, var_name, replacement))
                .collect(),
        ),
        Type::Negation(inner) => {
            Type::Negation(Box::new(substitute_recvar(inner, var_name, replacement)))
        }
        Type::App(f, a) => Type::App(
            Box::new(substitute_recvar(f, var_name, replacement)),
            Box::new(substitute_recvar(a, var_name, replacement)),
        ),
        Type::TypeStageApp { fn_name, args } => Type::TypeStageApp {
            fn_name: fn_name.clone(),
            args: args
                .iter()
                .map(|a| substitute_recvar(a, var_name, replacement))
                .collect(),
        },
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => Type::NominalVariant {
            tycon: tycon.clone(),
            ctor: ctor.clone(),
            fields: substitute_recvar_row(fields, var_name, replacement),
        },
        // All other variants (primitives, capabilities, leaf constructors, TypeVar with
        // a different name, Operator) contain no type variable positions — clone as-is.
        _ => ty.clone(),
    }
}

/// Row-level substitution helper for `substitute_recvar`.
///
/// Substitutes `TypeVar(var_name, _)` → `replacement` in all field values and the
/// row tail of a `Row`. Shared by the Record and NominalVariant arms.
pub(crate) fn substitute_recvar_row(row: &Row, var_name: &str, replacement: &Type) -> Row {
    Row {
        fields: row
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), substitute_recvar(v, var_name, replacement)))
            .collect(),
        tail: match &row.tail {
            RowTail::Empty => RowTail::Empty,
            RowTail::Uniform { key, value } => RowTail::Uniform {
                key: key
                    .as_ref()
                    .map(|k| Box::new(substitute_recvar(k, var_name, replacement))),
                value: Box::new(substitute_recvar(value, var_name, replacement)),
            },
        },
    }
}

/// Unfold a recursive type one step: replace all `TypeVar(var, _)` occurrences in `body`
/// with the full `Recursive` type. This is the standard equirecursive unfolding operation.
///
/// `μvar.body[var]` → `body[μvar.body/var]`
///
/// After unfolding, the former recursive positions in `body` hold the full `Recursive`
/// type again. When `is_atom_subtype (called via is_subtype_bas)` encounters those
/// positions, S-Assum fires immediately — the hypothesis `(v1, v2)` is already in sigma.
///
/// `unfold_once` is used only in subtype checking (S-Exp arm), where S-Assum prevents
/// divergence. It is NOT used in unification (which uses simultaneous opening instead).
///
/// If `rec` is not a `Type::Recursive`, it is returned unchanged (defensive).
// S-861: equirecursive-checker
pub fn unfold_once(rec: &Type) -> Type {
    match rec {
        Type::Recursive { var, body } => substitute_recvar(body, var, rec),
        _ => rec.clone(),
    }
}

/// Extract the root TyCon name and ordered argument list from a curried App chain.
///
/// `App(App(TyCon("Map"), K), V)` → `Some(("Map", [&K, &V]))`
/// `App(TyCon("F"), T)`           → `Some(("F", [&T]))`
/// `TyCon("Foo")`                 → `Some(("Foo", []))`  (zero-arity)
/// Any other form                 → `None`
///
/// Arguments are returned in application order (left-to-right): the leftmost parameter of
/// the original `[type Foo a b]` declaration is `args[0]`, the rightmost is `args[n-1]`.
pub(crate) fn extract_tycon_spine(ty: &Type) -> Option<(&str, Vec<&Type>)> {
    let mut args = Vec::new();
    let mut cur = ty;
    loop {
        match cur {
            Type::App(f, a) => {
                args.push(a.as_ref());
                cur = f;
            }
            Type::TyCon(name) => {
                args.reverse();
                return Some((name.as_str(), args));
            }
            _ => return None,
        }
    }
}

impl Type {
    // ── Error constructors and accessors ────────────────────────────────────────

    /// Construct an Error node carrying the errors that caused it.
    pub fn error_with(errs: Vec<TypeErrorTyped>) -> Self {
        Type::Error(Arc::new(errs))
    }

    /// Construct an Error node with a plain message string when no source span is available.
    /// Use for synthetic/internal failures where a meaningful description exists but no AST
    /// node to point at. Shows as `<error: {msg}>` in diagnostics.
    pub fn error_note(msg: impl Into<String>) -> Self {
        use crate::ast::Position;
        let zero = Position {
            offset: 0,
            line: 0,
            column: 0,
        };
        let span = Span {
            start: zero,
            end: zero,
            file: crate::rust_span!().file,
        };
        Type::Error(Arc::new(vec![TypeErrorTyped::Generic(GenericTypeError {
            message: msg.into(),
            span,
            notes: vec![],
            call_stack: vec![],
        })]))
    }

    /// Returns `true` if this type is an `Error` node (with or without payload).
    pub fn is_error(&self) -> bool {
        matches!(self, Type::Error(_))
    }

    /// Extract the causal errors from an Error node.
    /// Returns an empty slice for cascade sentinels or for non-Error types.
    pub fn error_payload(&self) -> &[TypeErrorTyped] {
        if let Type::Error(errs) = self {
            errs.as_slice()
        } else {
            &[]
        }
    }

    // ── Subtype / consistency relations ─────────────────────────────────────────

    /// Subtype relation with depth guard (defense-in-depth).
    ///
    /// Structural recursion on algebraic data types is safe (each call descends into a strict
    /// sub-term), and the occurs-check invariant (Robinson 1965) ensures substitution-applied
    /// types are acyclic. However, a depth guard prevents stack overflow on pathological cases.
    ///
    /// Post gradual-typing-split (B2): Top is the true supertype (τ <: Top for all τ). Unknown
    /// is NOT in the subtype lattice — Unknown relates to other types via consistency (~), not
    /// subtyping (<:). See is_consistent() for the consistency relation.
    ///
    /// BAS subtyping: `A <: B` iff `A & ~B` is uninhabited (RDNF emptiness check).
    ///
    /// Allocates the coinductive sigma context once per top-level call and threads it
    /// through all recursive calls. The sigma set records `(a.var, b.var)` pairs for
    /// `Recursive` types already under comparison — S-Assum fires (returns `true`) when
    /// the same pair is encountered again, preventing divergence on cyclic types.
    ///
    /// See: Chau & Parreaux (POPL 2026), Parreaux & Chau (OOPSLA 2022).
    pub fn is_subtype(
        sub: &Type,
        sup: &Type,
        tycon_env: Option<&crate::type_def::TyConEnv>,
    ) -> bool {
        let mut sigma: HashSet<(String, String)> = HashSet::new();
        Self::is_subtype_bas(sub, sup, tycon_env, &mut sigma)
    }

    /// BAS subtyping judgment: `A <: B` iff `A & ~B` is uninhabited, with a TypeVar exception.
    ///
    /// This is the core BAS algorithm (Chau & Parreaux, POPL 2026; Parreaux & Chau,
    /// OOPSLA 2022). The judgment is:
    ///
    ///   `A <: B`  iff  `is_empty(to_rdnf(A & ~B))`
    ///
    /// Converts the "difference type" `A & ~B` to Reduced Disjunctive Normal Form (RDNF),
    /// then checks emptiness. Emptiness = uninhabited = no value can be A but not B.
    ///
    /// `sigma` is the coinductive hypothesis set for Recursive types (S-Assum/S-Exp).
    /// Threaded through all recursive calls. Allocated once per top-level `is_subtype` call.
    ///
    /// ## Early guards (before RDNF)
    ///
    /// Several type forms short-circuit before RDNF normalization for efficiency and
    /// correctness:
    /// - Error: never a subtype of anything (sentinel for failed inference)
    /// - Top (Any): τ <: Top for all τ
    /// - Never: Never <: τ for all τ
    /// - Unknown: not in the subtype lattice (uses consistency instead)
    /// - TypeVar: **returns `true` unconditionally (see below)**
    ///
    /// ## TypeVar: approximate consistent-subtyping, not proper subtyping
    ///
    /// When either `sub` or `sup` is an unresolved `TypeVar`, this function returns `true`.
    /// This is NOT a proper BAS subtype judgment — it is a conservative approximation that
    /// defers the constraint to the unification/constraint solver (`constrain()`, `unify()`).
    ///
    /// **Rationale:** An unresolved TypeVar is an inference variable. At the point
    /// `is_subtype` is called, the substitution may not yet have been applied. Returning
    /// `true` avoids false rejections; the constraint solver handles the actual binding.
    ///
    /// **Consequence:** This makes `is_subtype` an APPROXIMATION of the true subtype relation
    /// when TypeVars are present — it is sound (no false negatives for ground types) but not
    /// complete (TypeVar cases are always accepted). Callers that need the precise relation
    /// for already-resolved types must apply the substitution before calling `is_subtype`.
    ///
    /// **Transitivity note (B-446):** Because TypeVar returns `true` on EITHER side,
    /// the function does NOT preserve proper subtype transitivity when TypeVars are present:
    ///   `is_subtype(TypeVar("a"), Int)` = `true`  AND
    ///   `is_subtype(Int, TypeVar("b"))` = `true`  AND
    ///   `is_subtype(TypeVar("a"), TypeVar("b"))` = `true`
    /// but this reflects the consistent-subtyping approximation, not a proof that any
    /// concrete instantiation of `a` is a subtype of any concrete instantiation of `b`.
    /// All TypeVar cases fire the guard and return `true` regardless of what the
    /// TypeVars will eventually be bound to. The solver enforces actual bounds.
    ///
    /// **Contrast with `Unknown`:** `Unknown` (the gradual `?`) returns `false` from
    /// `is_subtype` — it lives outside the subtype lattice and uses `is_consistent` instead.
    /// TypeVar is different: it is an inference variable expected to be solved to a concrete
    /// type. Returning `true` is safe here because inference variables are eliminated before
    /// type errors are reported to users.
    pub fn is_subtype_bas(
        sub: &Type,
        sup: &Type,
        tycon_env: Option<&crate::type_def::TyConEnv>,
        sigma: &mut HashSet<(String, String)>,
    ) -> bool {
        // Error is not a subtype of anything (not even itself), and nothing is a subtype of Error.
        // It is a sentinel for failed inference and should not satisfy any constraint.
        if matches!(sub, Type::Error(_)) || matches!(sup, Type::Error(_)) {
            return false;
        }

        // [S-TOP]: τ <: Top for all τ (Top is the supertype of everything)
        if matches!(sup, Type::Any) {
            return true;
        }

        // [S-NEVER]: Never <: τ for all τ (Never is the subtype of everything)
        if matches!(sub, Type::Never) {
            return true;
        }

        // Unknown is NOT a subtype of anything (except Top, handled above), and no type is a
        // subtype of Unknown. Unknown uses consistency, not subtyping. See is_consistent().
        if matches!(sub, Type::Unknown) || matches!(sup, Type::Unknown) {
            return false;
        }

        // TypeVar on either side → true (conservative approximation; see docstring).
        //
        // This is NOT a proper BAS subtype proof. An unresolved TypeVar is an inference
        // variable whose concrete type is determined by the constraint solver (constrain(),
        // unify()). We return `true` to avoid false rejections before substitution is applied.
        //
        // Callers that need the precise relation must apply the substitution FIRST and then
        // call is_subtype on the resolved types. See B-446 for the transitivity discussion.
        //
        // Note: this returns `true` even for two DIFFERENT TypeVars (TypeVar("a") <: TypeVar("b"))
        // which does NOT hold in general. The constraint solver enforces actual bounds; this
        // guard is a deferral, not a proof.
        if matches!(sub, Type::TypeVar(_, _)) || matches!(sup, Type::TypeVar(_, _)) {
            return true;
        }

        // Reflexivity short-circuit (avoids RDNF for the common case)
        if sub == sup {
            return true;
        }

        // BAS subtyping judgment: A <: B iff A & ~B is uninhabited.
        // Construct the difference type, convert to RDNF, check emptiness.
        let diff = Type::Intersection(vec![sub.clone(), Type::Negation(Box::new(sup.clone()))]);
        let rdnf = crate::bas::to_rdnf(&diff);
        crate::bas::is_rdnf_empty(&rdnf, tycon_env, sigma)
    }

    /// The AGT consistent subtyping relation (Garcia et al. 2016, Proposition 22): `A ~<: B`.
    ///
    /// Used for `value_matches_type`: ground types carry `Unknown` at erased positions
    /// (collection elements, Map values, Dict field values, Function params/returns).
    /// Plain `is_subtype` rejects `Unknown`; this relation treats `Unknown` as consistent
    /// with all types at any depth.
    ///
    /// **Structural recursion for compound types:** Every type constructor that `ground_type_of`
    /// can produce with `Unknown` at structural depth has an explicit arm here. The fallthrough
    /// to `is_subtype` is safe because it only handles types without structural sub-components
    /// or types that `ground_type_of` never produces.
    pub fn is_consistent_subtype(sub: &Type, sup: &Type) -> bool {
        // Unknown on either side: consistent (? ~<: T and T ~<: ? for all T)
        if matches!(sub, Type::Unknown) || matches!(sup, Type::Unknown) {
            return true;
        }
        // Unresolved TypeVar in annotation position: treat as Unknown (gradual)
        if matches!(sup, Type::TypeVar(_, _)) {
            return true;
        }
        // Error is never a consistent subtype of anything
        if matches!(sub, Type::Error(_)) || matches!(sup, Type::Error(_)) {
            return false;
        }
        match (sub, sup) {
            // Primitives: exact match
            (Type::Int, Type::Int)
            | (Type::Str, Type::Str)
            | (Type::Float, Type::Float)
            | (Type::Bytes, Type::Bytes) => true,
            // Top accepts everything
            (_, Type::Any) => true,
            // Structural recursion — consistent subtyping throughout all composite types.
            // App covers F[A] ~<: F[B] for any parameterized type constructor.
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                Self::is_consistent_subtype(f1, f2) && Self::is_consistent_subtype(a1, a2)
            }
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (Type::Dict(sub_row), Type::Dict(sup_row)) => {
                // Width subtyping: sub must supply every field sup requires.
                // Field types use consistent subtyping: Unknown field ~<: any annotation.
                sup_row.fields.iter().all(|(field, sup_ty)| {
                    sub_row
                        .fields
                        .get(field)
                        .map(|sub_ty| Self::is_consistent_subtype(sub_ty, sup_ty))
                        .unwrap_or(false) // field absent in sub → fails
                })
            }
            // Function: contravariant params, covariant return.
            // ground_type_of erases param/return types to Unknown; consistent subtyping
            // accepts Function([Unknown..], Unknown) against any concrete function annotation.
            (
                Type::Function {
                    params: sub_p,
                    ret: sub_r,
                    variadic: sub_v,
                    required_count: _,
                },
                Type::Function {
                    params: sup_p,
                    ret: sup_r,
                    variadic: sup_v,
                    required_count: _,
                },
            ) => {
                // Mirror the is_subtype "any function" special case: a zero-param variadic
                // function is the top of the function lattice (matches any callable).
                // This ensures `[@Fn f]` accepts lambdas of any arity at runtime, where
                // ground_type_of produces Function{params:[Unknown..], ret:Unknown} with
                // concrete param count that would otherwise fail the length equality check.
                let sup_is_any_fn = sup_p.is_empty() && *sup_v;
                if sup_is_any_fn {
                    // sub is any function → any function is consistent with top-of-fn-lattice
                    let sub_is_any_fn = sub_p.is_empty() && *sub_v;
                    if sub_is_any_fn {
                        return Self::is_consistent_subtype(sub_r, sup_r);
                    }
                    // Concrete-arity function ~<: any-function: always consistent
                    return true;
                }
                // B-454: variadic flag must match. A variadic function (collects rest args
                // into a rest parameter) has fundamentally different call semantics from a non-variadic
                // function with the same declared param count. Consistent subtyping cannot
                // paper over that difference: a caller that passes extra arguments to a
                // non-variadic function will fail at runtime, regardless of Unknown positions.
                sub_p.len() == sup_p.len()
                    && sub_v == sup_v
                    && sub_p.iter().zip(sup_p.iter()).all(
                        |((_sub_name, sub_ty), (_sup_name, sup_ty))| {
                            Self::is_consistent_subtype(sup_ty, sub_ty) // contravariant
                        },
                    )
                    && Self::is_consistent_subtype(sub_r, sup_r)
            }
            // Union in sub: all members must be c.s. subtype of sup.
            // Handles e.g. (Int | Str) ~<: Top, (Int | Unknown) ~<: Int.
            // Must appear before the wildcard `(_, Type::Union(...))` arm below.
            (Type::Union(members), _) => {
                members.iter().all(|m| Self::is_consistent_subtype(m, sup))
            }
            // Union in sup: value is c.s. subtype of union if c.s. subtype of any member
            (_, Type::Union(members)) => {
                members.iter().any(|m| Self::is_consistent_subtype(sub, m))
            }
            // Intersection in sup: value must be c.s. subtype of all members
            (_, Type::Intersection(members)) => {
                members.iter().all(|m| Self::is_consistent_subtype(sub, m))
            }
            // Remaining cases (NominalVariant, Handle at static level, etc.): fall to is_subtype.
            // Safe because ground_type_of never produces these with Unknown at structural depth
            // (Handle → Unknown, Variant fields → empty row).
            _ => Self::is_subtype(sub, sup, None),
        }
    }

    /// Check if two types are disjoint (have no values in common).
    ///
    /// Used for Negation subtyping: `A <: ~B` iff `types_are_disjoint(A, B)`.
    ///
    /// Returns true if the types provably have no overlap, false if they might overlap
    /// or we can't prove disjointness (conservative). This is NOT complete — it only
    /// catches obvious disjointness like `Int` vs `String`.
    pub fn types_are_disjoint(t1: &Type, t2: &Type) -> bool {
        // Never is disjoint from everything (it has no inhabitants)
        if matches!(t1, Type::Never) || matches!(t2, Type::Never) {
            return true;
        }

        // Unknown, Top, and Error are conservatively assumed to overlap with everything
        if matches!(t1, Type::Unknown | Type::Any | Type::Error(_))
            || matches!(t2, Type::Unknown | Type::Any | Type::Error(_))
        {
            return false;
        }

        // Different concrete primitives are disjoint
        match (t1, t2) {
            // Same type → not disjoint
            (a, b) if a == b => false,

            // Int and Float are disjoint (their supertype, not intersection)
            (Type::Int, Type::Float) | (Type::Float, Type::Int) => true,
            (Type::IntLiteral(_), Type::Float) | (Type::Float, Type::IntLiteral(_)) => true,

            // Different primitives are disjoint
            (Type::Int | Type::IntLiteral(_), Type::Str | Type::StringLiteral(_)) => true,
            (Type::Int | Type::IntLiteral(_), Type::Bytes) => true,
            (Type::Float, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Float, Type::Bytes) => true,
            (Type::Str | Type::StringLiteral(_), Type::Bytes) => true,

            // Symmetric cases
            (Type::Str | Type::StringLiteral(_), Type::Int | Type::IntLiteral(_)) => true,
            (Type::Bytes, Type::Int | Type::IntLiteral(_)) => true,
            (Type::Str | Type::StringLiteral(_), Type::Float) => true,
            (Type::Bytes, Type::Float) => true,
            (Type::Bytes, Type::Str | Type::StringLiteral(_)) => true,

            // Record vs any primitive is disjoint
            (Type::Dict(_), Type::Int | Type::IntLiteral(_)) => true,
            (Type::Dict(_), Type::Float) => true,
            (Type::Dict(_), Type::Str | Type::StringLiteral(_)) => true,
            (Type::Dict(_), Type::Bytes) => true,
            (Type::Int | Type::IntLiteral(_), Type::Dict(_)) => true,
            (Type::Float, Type::Dict(_)) => true,
            (Type::Str | Type::StringLiteral(_), Type::Dict(_)) => true,
            (Type::Bytes, Type::Dict(_)) => true,

            // Function vs primitives (for precise false-branch narrowing after function predicate guards)
            (Type::Function { .. }, Type::Int | Type::IntLiteral(_)) => true,
            (Type::Function { .. }, Type::Float) => true,
            (Type::Function { .. }, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Function { .. }, Type::Bytes) => true,
            (Type::Int | Type::IntLiteral(_), Type::Function { .. }) => true,
            (Type::Float, Type::Function { .. }) => true,
            (Type::Str | Type::StringLiteral(_), Type::Function { .. }) => true,
            (Type::Bytes, Type::Function { .. }) => true,

            // Function vs structural types (Record, NominalVariant, App)
            (Type::Function { .. }, Type::Dict(_)) => true,
            (Type::Function { .. }, Type::App(_, _)) => true,
            (Type::Function { .. }, Type::NominalVariant { .. }) => true,
            (Type::Dict(_), Type::Function { .. }) => true,
            (Type::App(_, _), Type::Function { .. }) => true,
            (Type::NominalVariant { .. }, Type::Function { .. }) => true,

            // NominalVariant vs primitives
            (Type::NominalVariant { .. }, Type::Int | Type::IntLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Float) => true,
            (Type::NominalVariant { .. }, Type::Str | Type::StringLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Bytes) => true,
            (Type::NominalVariant { .. }, Type::App(_, _)) => true,
            (Type::Int | Type::IntLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Float, Type::NominalVariant { .. }) => true,
            (Type::Str | Type::StringLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Bytes, Type::NominalVariant { .. }) => true,
            (Type::App(_, _), Type::NominalVariant { .. }) => true,

            // Union: disjoint if ALL members are disjoint from the other type
            (Type::Union(members), t) | (t, Type::Union(members)) => {
                members.iter().all(|m| Type::types_are_disjoint(m, t))
            }

            // Intersection: disjoint if ANY member is disjoint from the other type
            (Type::Intersection(members), t) | (t, Type::Intersection(members)) => {
                members.iter().any(|m| Type::types_are_disjoint(m, t))
            }

            // Two single-field CLOSED records with DIFFERENT keys are disjoint (S-RcdTop).
            // {x: T} and {y: U} where x ≠ y have no values in common — no record can
            // satisfy both field requirements. This improves Negation subtyping precision
            // without requiring full RDNF normalization.
            // Records with Uniform tails are open and do not satisfy S-RcdTop disjointness.
            (Type::Dict(row1), Type::Dict(row2))
                if row1.fields.len() == 1
                    && row2.fields.len() == 1
                    && row1.tail == RowTail::Empty
                    && row2.tail == RowTail::Empty =>
            {
                let key1 = row1.fields.keys().next().unwrap();
                let key2 = row2.fields.keys().next().unwrap();
                key1 != key2
            }

            // TypeStageApp might overlap with anything (conservative)
            (Type::TypeStageApp { .. }, _) | (_, Type::TypeStageApp { .. }) => false,
            // NominalVariant with different tags are disjoint (nominal disjointness)
            (
                Type::NominalVariant {
                    tycon: tycon1,
                    ctor: ctor1,
                    ..
                },
                Type::NominalVariant {
                    tycon: tycon2,
                    ctor: ctor2,
                    ..
                },
            ) => tycon1 != tycon2 || ctor1 != ctor2,
            // NominalVariant vs Record (both directions)
            (Type::NominalVariant { .. }, Type::Dict(_)) => true,
            (Type::Dict(_), Type::NominalVariant { .. }) => true,
            // Conservative: assume all other combinations might overlap
            _ => false,
        }
    }

    /// Consistency relation for gradual typing (Siek & Taha 2006, Garcia et al. 2016 AGT).
    ///
    /// The consistency relation (~) is used for Unknown types. Key properties:
    /// - Reflexive: τ ~ τ for all τ
    /// - Symmetric: τ₁ ~ τ₂ ⟺ τ₂ ~ τ₁
    /// - NOT transitive: Int ~ Unknown and Unknown ~ Str, but NOT Int ~ Str
    ///
    /// This non-transitivity prevents Unknown from collapsing all types into equivalence,
    /// which was the problem with Any-as-top-and-bottom.
    ///
    /// Consistency decomposes structurally: Fn(τ₁ → τ₂) ~ Fn(σ₁ → σ₂) iff τ₁ ~ σ₁ and τ₂ ~ σ₂.
    /// Records require shared fields to be consistent; differing field sets are consistent
    /// (no width restriction).
    pub fn is_consistent(a: &Type, b: &Type) -> bool {
        // Unknown is consistent with everything
        if matches!(a, Type::Unknown) || matches!(b, Type::Unknown) {
            return true;
        }
        // Reflexive: τ ~ τ for all concrete types
        if a == b {
            return true;
        }
        // Error is not consistent with anything (sentinel for failed inference)
        if matches!(a, Type::Error(_)) || matches!(b, Type::Error(_)) {
            return false;
        }
        // Structural decomposition
        match (a, b) {
            // App covers F[A] ~ F[B] for any parameterized type constructor.
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                Type::is_consistent(f1, f2) && Type::is_consistent(a1, a2)
            }
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                    variadic: v1,
                    required_count: _,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                    variadic: v2,
                    required_count: _,
                },
            ) => {
                // Special case: any-function (Function{params:[], variadic:true}) is consistent
                // with all function types under gradual typing (Garcia et al. 2016).
                let a_is_any_fn = p1.is_empty() && *v1;
                let b_is_any_fn = p2.is_empty() && *v2;
                if a_is_any_fn || b_is_any_fn {
                    return true;
                }

                // Normal structural consistency for concrete function types
                v1 == v2
                    && p1.len() == p2.len()
                    && p1
                        .iter()
                        .zip(p2.iter())
                        .all(|((_n1, ty1), (_n2, ty2))| Type::is_consistent(ty1, ty2))
                    && Type::is_consistent(r1, r2)
            }
            (Type::Dict(row1), Type::Dict(row2)) => {
                // Shared fields must be consistent
                for (k, ty1) in &row1.fields {
                    if let Some(ty2) = row2.fields.get(k) {
                        if !Type::is_consistent(ty1, ty2) {
                            return false;
                        }
                    }
                }
                // Differing fields are OK (width subtyping-like, but symmetric).
                // Tails: both Uniform → value types must be consistent.
                // Empty vs Uniform → consistent (Uniform adds constraint, but ? is open).
                match (&row1.tail, &row2.tail) {
                    (RowTail::Empty, RowTail::Empty) => {}
                    (RowTail::Uniform { value: v1, .. }, RowTail::Uniform { value: v2, .. })
                        if !Type::is_consistent(v1, v2) =>
                    {
                        return false;
                    }
                    // One Empty, one Uniform — consistent (Unknown-style open matching)
                    _ => {}
                }
                true
            }
            (Type::Union(members1), Type::Union(members2)) => {
                // Union ~ Union iff for each member in one, there's a consistent member in the other
                // This is symmetric and handles partial overlap
                members1
                    .iter()
                    .all(|m1| members2.iter().any(|m2| Type::is_consistent(m1, m2)))
                    && members2
                        .iter()
                        .all(|m2| members1.iter().any(|m1| Type::is_consistent(m1, m2)))
            }
            (Type::Intersection(members1), Type::Intersection(members2)) => {
                // Intersection ~ Intersection iff for each member in one, there's a consistent member in the other
                // This mirrors the union case
                members1
                    .iter()
                    .all(|m1| members2.iter().any(|m2| Type::is_consistent(m1, m2)))
                    && members2
                        .iter()
                        .all(|m2| members1.iter().any(|m1| Type::is_consistent(m1, m2)))
            }
            // Record ~ Intersection-of-Records: consistent if shared fields are consistent.
            // Multi-field annotations `@[f1: T1  f2: T2]` resolve to
            // `Intersection([{f1: T1, ...ρ1}, {f2: T2, ...ρ2}])`.  Checking a concrete
            // record (possibly containing Unknown) against this intersection should succeed
            // when all intersection members' known fields are individually consistent with
            // the corresponding record fields.  This mirrors the `Record ~ Record` case
            // (which only checks shared fields) applied per member.
            (Type::Dict(row), Type::Intersection(members))
            | (Type::Intersection(members), Type::Dict(row)) => members.iter().all(|m| {
                if let Type::Dict(mrow) = m {
                    // Check shared fields between the record and this member
                    for (k, mt) in &mrow.fields {
                        if let Some(rt) = row.fields.get(k) {
                            if !Type::is_consistent(rt, mt) {
                                return false;
                            }
                        }
                        // Field present in member but not in record is OK — open rows absorb
                    }
                    true
                } else {
                    // Non-Record member — fall back to structural consistency
                    Type::is_consistent(&Type::Dict(row.clone()), m)
                }
            }),
            // Literal types are consistent with their parent types (similar to subtyping)
            (Type::IntLiteral(_), Type::Int) | (Type::Int, Type::IntLiteral(_)) => true,
            (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => true,
            // Top is consistent with everything (τ ~ Top for all τ)
            (Type::Any, _) | (_, Type::Any) => true,
            // Never is vacuously consistent with everything — Never is uninhabited, so no
            // runtime value can violate the consistency relation. This is not AGT gradual
            // consistency; it is vacuous truth.
            (Type::Never, _) | (_, Type::Never) => true,
            // TypeVar consistency: SOUND reflexivity check only.
            // Two TypeVars are consistent if they have the same name (same variable).
            // TypeVar vs concrete type is NOT consistent — callers must apply substitution first.
            (Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2,
            // Negation: structurally consistent
            (Type::Negation(t1), Type::Negation(t2)) => Type::is_consistent(t1, t2),
            // Negation vs concrete type: consistent if the types are disjoint.
            // If A is disjoint from B, then A ~ ~B (A is consistent with "not B").
            (Type::Negation(inner), other) | (other, Type::Negation(inner)) => {
                Type::types_are_disjoint(other, inner)
            }
            // TypeStageApp is consistent with everything (pending computation)
            (Type::TypeStageApp { .. }, _) | (_, Type::TypeStageApp { .. }) => true,
            // NominalVariant: consistent iff tags match and fields are structurally consistent
            (
                Type::NominalVariant {
                    tycon: tycon1,
                    ctor: ctor1,
                    fields: fields1,
                },
                Type::NominalVariant {
                    tycon: tycon2,
                    ctor: ctor2,
                    fields: fields2,
                },
            ) => {
                if tycon1 != tycon2 || ctor1 != ctor2 {
                    return false;
                }
                for (k, ty1) in &fields1.fields {
                    if let Some(ty2) = fields2.fields.get(k) {
                        if !Type::is_consistent(ty1, ty2) {
                            return false;
                        }
                    }
                }
                true
            }
            // Capability types, Proxy: consistent only if equal (handled by a == b above)
            // All other combinations are inconsistent
            _ => false,
        }
    }

    /// Check S-RcdTop: does the union contain two closed single-field Records with disjoint keys?
    /// Returns Some(()) if the union simplifies to Top, None otherwise.
    fn check_s_rcd_top(members: &[Type]) -> Option<()> {
        // S-RcdTop (Chau & Parreaux, POPL 2026): {x: tau} | {y: pi} = Top
        // Requires ALL members to be single-field records with pairwise disjoint field names.
        // A union like Union([Float, {x: Int}, {y: Str}]) must NOT trigger this rule
        // because Float is not a single-field record.
        if members.len() < 2 {
            return None;
        }
        // Guard: every member must be a single-field CLOSED record (RowTail::Empty).
        // Records with Uniform tails are open and do not satisfy S-RcdTop.
        let single_field_keys: Vec<&str> = members
            .iter()
            .map(|m| {
                if let Type::Dict(row) = m {
                    if row.fields.len() == 1 && row.tail == RowTail::Empty {
                        return row.fields.keys().next().map(|k| k.as_str());
                    }
                }
                None
            })
            .collect::<Option<Vec<_>>>()?;

        // Need at least two with different field names
        for i in 0..single_field_keys.len() {
            for j in (i + 1)..single_field_keys.len() {
                if single_field_keys[i] != single_field_keys[j] {
                    return Some(());
                }
            }
        }
        None
    }

    /// Check S-ClsBot: does the intersection contain two closed single-field Records with
    /// different field names?  Such a value cannot exist -> Never.
    /// S-ClsBot (Chau & Parreaux, POPL 2026): {x: tau} & {y: pi} = Never when x != y.
    /// Requires ALL members to be single-field records. An intersection like
    /// Intersection([Int, {x: T}, {y: U}]) must NOT trigger this rule.
    fn check_s_cls_bot(members: &[Type]) -> bool {
        if members.len() < 2 {
            return false;
        }
        // Guard: every member must be a single-field CLOSED record (RowTail::Empty).
        // Records with Uniform tails are open and do not satisfy S-ClsBot.
        let single_field_keys: Option<Vec<&str>> = members
            .iter()
            .map(|m| {
                if let Type::Dict(row) = m {
                    if row.fields.len() == 1 && row.tail == RowTail::Empty {
                        return row.fields.keys().next().map(|k| k.as_str());
                    }
                }
                None
            })
            .collect();
        let Some(keys) = single_field_keys else {
            return false;
        };

        // If there are two entries with different names, the intersection is uninhabited
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                if keys[i] != keys[j] {
                    return true;
                }
            }
        }
        false
    }

    pub fn collect_type_vars(&self, vars: &mut HashSet<String>) {
        match self {
            Type::TypeVar(name, _) => {
                vars.insert(name.clone());
            }
            Type::Dict(row) => {
                for ty in row.fields.values() {
                    ty.collect_type_vars(vars);
                }
                // Collect type variables from RowTail::Uniform's key and value types
                if let RowTail::Uniform { key, value } = &row.tail {
                    if let Some(k) = key {
                        k.collect_type_vars(vars);
                    }
                    value.collect_type_vars(vars);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_type_vars(vars);
                }
                ret.collect_type_vars(vars);
            }
            Type::Union(members) => {
                for member in members {
                    member.collect_type_vars(vars);
                }
            }
            Type::Intersection(members) => {
                for member in members {
                    member.collect_type_vars(vars);
                }
            }
            Type::TypeStageApp { fn_name: _, args } => {
                for arg in args {
                    arg.collect_type_vars(vars);
                }
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => {
                for ty in fields.fields.values() {
                    ty.collect_type_vars(vars);
                }
            }
            Type::TyCon(_) => {} // TyCon has no type variables
            // S-860: equirecursive-types-core — recurse into the body.
            // TypeVars inside a Recursive body must be collected for generalization.
            Type::Recursive { var: _, body } => body.collect_type_vars(vars),
            _ => {}
        }
    }

    /// Returns true if the type contains any inference variables (TypeVar).
    /// Used to determine whether a type is concrete or still under inference.
    pub fn has_inference_vars(&self) -> bool {
        match self {
            Type::TypeVar(_, _) => true,
            Type::Dict(row) => {
                row.fields.values().any(|ty| ty.has_inference_vars())
                    || match &row.tail {
                        RowTail::Empty => false,
                        RowTail::Uniform { key, value } => {
                            key.as_ref().is_some_and(|k| k.has_inference_vars())
                                || value.has_inference_vars()
                        }
                    }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                params.iter().any(|(_name, p_ty)| p_ty.has_inference_vars())
                    || ret.has_inference_vars()
            }
            Type::Union(members) => members.iter().any(|m| m.has_inference_vars()),
            Type::Intersection(members) => members.iter().any(|m| m.has_inference_vars()),
            Type::Negation(inner) => inner.has_inference_vars(),
            Type::App(f, a) => f.has_inference_vars() || a.has_inference_vars(),
            Type::TyCon(_) => false, // TyCon is a concrete named constructor, not a variable
            Type::Operator(_) => true, // Operator variables ARE inference variables
            Type::TypeStageApp { fn_name: _, args } => {
                args.iter().any(|arg| arg.has_inference_vars())
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => fields.fields.values().any(|ty| ty.has_inference_vars()),
            Type::Proxy => false,
            // S-860: equirecursive-types-core — recurse into the body.
            // A Recursive type with inference vars in the body is not yet fully concrete.
            Type::Recursive { var: _, body } => body.has_inference_vars(),
            _ => false,
        }
    }

    /// Check if this type contains any TypeStageApp nodes.
    /// Used to determine if deferred equalities can be resolved.
    pub fn has_type_stage_app(&self) -> bool {
        match self {
            Type::TypeStageApp { .. } => true,
            Type::Dict(row) => {
                row.fields.values().any(|ty| ty.has_type_stage_app())
                    || match &row.tail {
                        RowTail::Empty => false,
                        RowTail::Uniform { key, value } => {
                            key.as_ref().is_some_and(|k| k.has_type_stage_app())
                                || value.has_type_stage_app()
                        }
                    }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                params.iter().any(|(_name, p_ty)| p_ty.has_type_stage_app())
                    || ret.has_type_stage_app()
            }
            Type::Union(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Intersection(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Negation(inner) => inner.has_type_stage_app(),
            Type::App(f, a) => f.has_type_stage_app() || a.has_type_stage_app(),
            Type::TyCon(_) => false,
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => fields.fields.values().any(|ty| ty.has_type_stage_app()),
            // S-860: equirecursive-types-core — recurse into the body.
            Type::Recursive { var: _, body } => body.has_type_stage_app(),
            _ => false,
        }
    }

    /// Collect type variables in a single tree walk.
    pub fn collect_all_vars(&self, type_vars: &mut HashSet<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.insert(name.clone());
            }
            Type::Dict(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars(type_vars);
                }
                // Collect type variables from RowTail::Uniform's key and value types
                if let RowTail::Uniform { key, value } = &row.tail {
                    if let Some(k) = key {
                        k.collect_all_vars(type_vars);
                    }
                    value.collect_all_vars(type_vars);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_all_vars(type_vars);
                }
                ret.collect_all_vars(type_vars);
            }
            Type::Union(members) => {
                for member in members {
                    member.collect_all_vars(type_vars);
                }
            }
            Type::Intersection(members) => {
                for member in members {
                    member.collect_all_vars(type_vars);
                }
            }
            Type::Negation(inner) => {
                inner.collect_all_vars(type_vars);
            }
            Type::App(f, a) => {
                f.collect_all_vars(type_vars);
                a.collect_all_vars(type_vars);
            }
            Type::TyCon(_) => {} // TyCon has no type variables
            Type::Operator(name) => {
                type_vars.insert(name.clone());
            }
            Type::TypeStageApp { fn_name: _, args } => {
                for arg in args {
                    arg.collect_all_vars(type_vars);
                }
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => {
                for ty in fields.fields.values() {
                    ty.collect_all_vars(type_vars);
                }
            }
            // S-860: equirecursive-types-core — recurse into the body.
            // TypeVars inside a Recursive body must be collected for generalization/instantiation.
            Type::Recursive { var: _, body } => body.collect_all_vars(type_vars),
            _ => {}
        }
    }

    /// Fused occurs check + variable collection: checks whether `occurs_name` appears
    /// in the type tree and simultaneously collects all type vars.
    /// Returns `true` if `occurs_name` was found (infinite-type guard for U-VAR arms).
    ///
    /// This fuses the double-walk of calling `type_var_occurs()` then
    /// `collect_all_vars()` separately in each U-VAR arm of `unify()`.
    pub fn collect_all_vars_check_occurs(
        &self,
        occurs_name: &str,
        type_vars: &mut HashSet<String>,
    ) -> bool {
        match self {
            Type::TypeVar(name, _) => {
                let found = name == occurs_name;
                type_vars.insert(name.clone());
                found
            }
            Type::Dict(row) => {
                let mut found = false;
                for ty in row.fields.values() {
                    found |= ty.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                // Check RowTail::Uniform's key and value types
                if let RowTail::Uniform { key, value } = &row.tail {
                    if let Some(k) = key {
                        found |= k.collect_all_vars_check_occurs(occurs_name, type_vars);
                    }
                    found |= value.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                let mut found = false;
                for (_name, p_ty) in params {
                    found |= p_ty.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found |= ret.collect_all_vars_check_occurs(occurs_name, type_vars);
                found
            }
            Type::Union(members) => {
                let mut found = false;
                for member in members {
                    found |= member.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found
            }
            Type::Intersection(members) => {
                let mut found = false;
                for member in members {
                    found |= member.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found
            }
            Type::Negation(inner) => inner.collect_all_vars_check_occurs(occurs_name, type_vars),
            Type::App(f, a) => {
                let mut found = false;
                found |= f.collect_all_vars_check_occurs(occurs_name, type_vars);
                found |= a.collect_all_vars_check_occurs(occurs_name, type_vars);
                found
            }
            Type::Operator(name) => {
                let found = name == occurs_name;
                type_vars.insert(name.clone());
                found
            }
            Type::TypeStageApp { fn_name: _, args } => {
                let mut found = false;
                for arg in args {
                    found |= arg.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => {
                let mut found = false;
                for ty in fields.fields.values() {
                    found |= ty.collect_all_vars_check_occurs(occurs_name, type_vars);
                }
                found
            }
            Type::TyCon(_) => false, // TyCon has no type variables
            // S-860: equirecursive-types-core — recurse into the body.
            // The `var` binder is a μ-binder, not an inference var, and must not
            // be checked for `occurs_name` or added to `type_vars`.
            Type::Recursive { var: _, body } => {
                body.collect_all_vars_check_occurs(occurs_name, type_vars)
            }
            _ => false,
        }
    }

    /// Collect type variables into Vecs, allowing duplicates. Cheaper than HashSet
    /// allocation; callers that need deduplication handle it via seen-set or contains_key guards.
    /// Production callers: `instantiate_at_level` and `generalize`. (The test-only `instantiate()`
    /// uses the HashSet variant `collect_all_vars` instead.)
    pub fn collect_all_vars_vec(&self, type_vars: &mut Vec<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.push(name.clone());
            }
            Type::Dict(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars_vec(type_vars);
                }
                // Collect type variables from RowTail::Uniform's key and value types
                if let RowTail::Uniform { key, value } = &row.tail {
                    if let Some(k) = key {
                        k.collect_all_vars_vec(type_vars);
                    }
                    value.collect_all_vars_vec(type_vars);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_all_vars_vec(type_vars);
                }
                ret.collect_all_vars_vec(type_vars);
            }
            Type::Union(members) => {
                for member in members {
                    member.collect_all_vars_vec(type_vars);
                }
            }
            Type::Intersection(members) => {
                for member in members {
                    member.collect_all_vars_vec(type_vars);
                }
            }
            Type::Negation(inner) => {
                inner.collect_all_vars_vec(type_vars);
            }
            Type::App(f, a) => {
                f.collect_all_vars_vec(type_vars);
                a.collect_all_vars_vec(type_vars);
            }
            Type::Operator(name) => {
                type_vars.push(name.clone());
            }
            Type::TypeStageApp { fn_name: _, args } => {
                for arg in args {
                    arg.collect_all_vars_vec(type_vars);
                }
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => {
                for ty in fields.fields.values() {
                    ty.collect_all_vars_vec(type_vars);
                }
            }
            // S-860: equirecursive-types-core — recurse into the body.
            // The `var` binder name is a gensym'd μ-binder, NOT an inference TypeVar, and
            // must NOT be added to `type_vars`. Only the body is walked for TypeVars.
            // Not recursing would make TypeVars inside a Recursive body invisible to
            // generalization — a soundness gap per the design review (agent_type-theorist).
            Type::Recursive { var: _, body } => {
                body.collect_all_vars_vec(type_vars);
            }
            // Exhaustive leaf enumeration — no wildcard to prevent silently missing new compound variants
            Type::Int
            | Type::IntLiteral(_)
            | Type::Float
            | Type::Str
            | Type::StringLiteral(_)
            | Type::Bytes
            | Type::Proxy
            | Type::Unknown
            | Type::Any
            | Type::Error(_)
            | Type::DirCap
            | Type::NetCap
            | Type::TyCon(_)
            | Type::Uri
            | Type::Timestamp
            | Type::Duration
            | Type::ClockCap
            | Type::Timezone
            | Type::QuicSession
            | Type::Http2Session
            | Type::Http3Session
            | Type::QuicDatagramHandle
            | Type::DatagramHandle
            | Type::Never => {}
        }
    }

    /// Collect all Operator variable names from this type.
    /// Used by instantiate_at_level to preserve Operator kind during instantiation.
    pub fn collect_operator_names(&self, operator_names: &mut HashSet<String>) {
        match self {
            Type::Operator(name) => {
                operator_names.insert(name.clone());
            }
            Type::Dict(row) => {
                for ty in row.fields.values() {
                    ty.collect_operator_names(operator_names);
                }
                // Collect Operator names from RowTail::Uniform key and value types
                if let RowTail::Uniform { key, value } = &row.tail {
                    if let Some(k) = key {
                        k.collect_operator_names(operator_names);
                    }
                    value.collect_operator_names(operator_names);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
                required_count: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_operator_names(operator_names);
                }
                ret.collect_operator_names(operator_names);
            }
            Type::Union(members) | Type::Intersection(members) => {
                for member in members {
                    member.collect_operator_names(operator_names);
                }
            }
            Type::Negation(inner) => {
                inner.collect_operator_names(operator_names);
            }
            Type::App(f, a) => {
                f.collect_operator_names(operator_names);
                a.collect_operator_names(operator_names);
            }
            Type::TypeStageApp { fn_name: _, args } => {
                for arg in args {
                    arg.collect_operator_names(operator_names);
                }
            }
            Type::NominalVariant {
                tycon: _,
                ctor: _,
                fields,
            } => {
                for ty in fields.fields.values() {
                    ty.collect_operator_names(operator_names);
                }
            }
            Type::TyCon(_) => {} // TyCon is a concrete constructor, not an Operator variable
            // S-860: equirecursive-types-core — recurse into the body.
            Type::Recursive { var: _, body } => body.collect_operator_names(operator_names),
            _ => {}
        }
    }

    /// Normalize a union type by flattening nested unions and removing duplicates.
    ///
    /// `Int | (Str | Bool)` becomes `Int | Str | Bool`.
    /// `Int | Int` becomes `Int`.
    pub fn normalize_union(members: Vec<Type>) -> Type {
        if members.is_empty() {
            panic!("normalize_union: empty union not allowed");
        }

        let mut flattened = Vec::new();
        for member in members {
            match member {
                Type::Union(nested) => {
                    // Flatten nested unions
                    flattened.extend(nested);
                }
                // Top absorbs all in union: T | Top = Top
                Type::Any => return Type::Any,
                // Unknown absorbs all in gradual union: T | Unknown = Unknown (AGT: ? ∪ T = ?)
                // Without this, Unknown | TypeVar(a) cannot be unified with TypeVar(a) due
                // to the occurs check (a appears in the union), causing spurious infinite type errors.
                Type::Unknown => return Type::Unknown,
                // Never is the identity in union: T | Never = T — skip it
                Type::Never => continue,
                _ => {
                    flattened.push(member);
                }
            }
        }

        // If all members were Never (identity), the union is empty — which is Never
        if flattened.is_empty() {
            return Type::Never;
        }

        // Deduplicate by collecting into a set and back to a vec
        let mut unique = Vec::new();
        for ty in flattened {
            if !unique.contains(&ty) {
                unique.push(ty);
            }
        }

        // Sort for canonical representation
        unique.sort_by(|a, b| {
            use std::cmp::Ordering;
            // Stable ordering based on type variant discriminant + payload
            let order_a = type_order(a);
            let order_b = type_order(b);
            match order_a.cmp(&order_b) {
                Ordering::Equal => type_payload_cmp(a, b),
                other => other,
            }
        });

        // Single-element unions unwrap to the bare type
        if unique.len() == 1 {
            unique.into_iter().next().unwrap()
        } else {
            Type::Union(unique)
        }
    }

    /// Normalize an intersection type: flatten, deduplicate, sort, and apply identity/absorbing rules.
    /// - Top is the identity: T & Top = T
    /// - Never is absorbing: T & Never = Never (bottom annihilates all in intersection)
    /// - Error is absorbing: T & Error = Error (sentinel for failed inference)
    /// - Single-element intersections unwrap to the bare type
    pub fn normalize_intersection(members: Vec<Type>) -> Type {
        if members.is_empty() {
            panic!("normalize_intersection: empty intersection not allowed");
        }

        // Error is absorbing: any intersection containing Error becomes Error.
        // Propagate all error payloads from Error members to preserve context.
        if members.iter().any(|m| matches!(m, Type::Error(_))) {
            let payloads: Vec<TypeErrorTyped> = members
                .iter()
                .flat_map(|m| m.error_payload().to_vec())
                .collect();
            return if payloads.is_empty() {
                Type::error_note("type error in intersection")
            } else {
                Type::error_with(payloads)
            };
        }

        // Never is absorbing: T & Never = Never (S-ClsBot base case: bottom annihilates)
        if members.iter().any(|m| matches!(m, Type::Never)) {
            return Type::Never;
        }

        let mut flattened = Vec::new();
        for member in members {
            match member {
                Type::Intersection(nested) => {
                    // Flatten nested intersections
                    flattened.extend(nested);
                }
                Type::Any => {
                    // Top is the identity: T & Top = T, so skip it
                    continue;
                }
                Type::Unknown => {
                    // Unknown is the identity in intersection under AGT (Garcia et al. 2016):
                    // T & ? = T. The gradual type ? acts as dynamic/Top in intersection contexts.
                    // This ensures [let n@Int] on an Unknown scrutinee gives n : Int, not n : Int & ?.
                    continue;
                }
                _ => {
                    flattened.push(member);
                }
            }
        }

        // If all members were Top (identity), return Top
        if flattened.is_empty() {
            return Type::Any;
        }

        // Deduplicate
        let mut unique = Vec::new();
        for ty in flattened {
            if !unique.contains(&ty) {
                unique.push(ty);
            }
        }

        // Sort for canonical representation
        unique.sort_by(|a, b| {
            let order_a = type_order(a);
            let order_b = type_order(b);
            match order_a.cmp(&order_b) {
                std::cmp::Ordering::Equal => type_payload_cmp(a, b),
                other => other,
            }
        });

        // Single-element intersections unwrap to the bare type
        if unique.len() == 1 {
            unique.into_iter().next().unwrap()
        } else {
            Type::Intersection(unique)
        }
    }

    /// Simplify a type by reducing trivial unions/intersections and applying algebraic laws.
    ///
    /// Rules applied:
    /// - Single-element union/intersection unwrapping
    /// - Never absorption in intersection, Top absorption in union
    /// - Never removal from union, Top removal from intersection
    /// - Literal promotion: 2+ IntLiterals → Int, 2+ StringLiterals → Str
    /// - Subsumption elimination: if A <: B for two members, drop A
    /// - S-RcdTop / S-ClsBot structural rules
    pub fn simplify_type(ty: Type) -> Type {
        // Bottom-up pass: simplify children first, then apply top-level rules.
        // This ensures that e.g. Union([Union([Int, Int]), Str]) fully collapses.
        let ty = Self::simplify_children(ty);

        match ty {
            // Single-element union/intersection — identity
            Type::Union(members) if members.len() == 1 => {
                // Unwrap and recursively simplify
                Type::simplify_type(members.into_iter().next().unwrap())
            }
            Type::Intersection(members) if members.len() == 1 => {
                Type::simplify_type(members.into_iter().next().unwrap())
            }
            // Never absorbs all in intersection: T & Never = Never
            Type::Intersection(ref members) if members.iter().any(|m| matches!(m, Type::Never)) => {
                Type::Never
            }
            // Top absorbs all in union: T | Top = Top
            Type::Union(ref members) if members.iter().any(|m| matches!(m, Type::Any)) => Type::Any,
            // Remove Never arms from union: T | Never = T
            Type::Union(members) if members.iter().any(|m| matches!(m, Type::Never)) => {
                let filtered: Vec<Type> = members
                    .into_iter()
                    .filter(|m| !matches!(m, Type::Never))
                    .collect();
                if filtered.is_empty() {
                    Type::Never
                } else {
                    Type::normalize_union(filtered)
                }
            }
            // Literal promotion: Union of multiple IntLiterals → replace with Int.
            // Union of multiple StringLiterals → replace with Str.
            // This mirrors the [U-SUBSUME] rule for literal types: IntLiteral(n) <: Int, so
            // any union of IntLiterals can be widened to Int. Applied when the union contains
            // 2+ distinct IntLiterals (or StringLiterals) so that infer_if's branch joins
            // produce a clean type rather than a collection of literals.
            // E.g., Union([IntLiteral(0), IntLiteral(42)]) → Int (via this rule + subsumption).
            Type::Union(members)
                if members
                    .iter()
                    .filter(|m| matches!(m, Type::IntLiteral(_)))
                    .count()
                    >= 2 =>
            {
                // Replace all IntLiterals with Int, then re-normalize
                let promoted: Vec<Type> = members
                    .into_iter()
                    .map(|m| {
                        if matches!(m, Type::IntLiteral(_)) {
                            Type::Int
                        } else {
                            m
                        }
                    })
                    .collect();
                Type::simplify_type(Type::normalize_union(promoted))
            }
            Type::Union(members)
                if members
                    .iter()
                    .filter(|m| matches!(m, Type::StringLiteral(_)))
                    .count()
                    >= 2 =>
            {
                // Replace all StringLiterals with Str, then re-normalize
                let promoted: Vec<Type> = members
                    .into_iter()
                    .map(|m| {
                        if matches!(m, Type::StringLiteral(_)) {
                            Type::Str
                        } else {
                            m
                        }
                    })
                    .collect();
                Type::simplify_type(Type::normalize_union(promoted))
            }
            // Subsumption elimination: if A <: B for two members, drop A (B covers it).
            // This collapses e.g. Union([Int, IntLiteral(0)]) → Int since IntLiteral(0) <: Int.
            // Conditions:
            // 1. No inference variables (concrete types only) — avoid eliminating free TypeVars.
            // 2. Supertype is not Negation — the conservative (_, Negation(_)) => true rule in
            //    is_subtype is an approximation and must not drive subsumption elimination.
            // 3. At least one pairwise (A, B) where A <: B and B is not Negation.
            Type::Union(members)
                if members.iter().all(|m| !m.has_inference_vars()) && {
                    members.iter().enumerate().any(|(i, a)| {
                        members.iter().enumerate().any(|(j, b)| {
                            i != j
                                && !matches!(b, Type::Negation(_))
                                && Type::is_subtype(a, b, None)
                        })
                    })
                } =>
            {
                // Remove members that are strict subtypes of another non-Negation member
                let mut to_keep: Vec<bool> = vec![true; members.len()];
                for i in 0..members.len() {
                    if !to_keep[i] {
                        continue;
                    }
                    for j in 0..members.len() {
                        if i == j || !to_keep[j] {
                            continue;
                        }
                        // Skip if supertype candidate is Negation (conservative rule not sound here)
                        if matches!(members[j], Type::Negation(_)) {
                            continue;
                        }
                        // If members[i] <: members[j], remove members[i]
                        if Type::is_subtype(&members[i], &members[j], None) {
                            to_keep[i] = false;
                            break;
                        }
                    }
                }
                let reduced: Vec<Type> = members
                    .into_iter()
                    .zip(to_keep)
                    .filter_map(|(m, keep)| if keep { Some(m) } else { None })
                    .collect();
                if reduced.is_empty() {
                    Type::Never
                } else {
                    Type::normalize_union(reduced)
                }
            }
            // S-RcdTop: union of closed single-field records with disjoint field names → Top
            Type::Union(members) => {
                if Self::check_s_rcd_top(&members).is_some() {
                    Type::Any
                } else {
                    Type::Union(members)
                }
            }
            // S-ClsBot: intersection of closed single-field records with different field names → Never
            Type::Intersection(members) => {
                if Self::check_s_cls_bot(&members) {
                    Type::Never
                } else {
                    Type::Intersection(members)
                }
            }
            // All other types are already in simplified form
            _ => ty,
        }
    }

    /// Recursively simplify all children of a compound type (bottom-up pass).
    /// Does NOT apply top-level simplification rules — that is done by `simplify_type`.
    fn simplify_children(ty: Type) -> Type {
        match ty {
            Type::Union(members) => {
                Type::Union(members.into_iter().map(Type::simplify_type).collect())
            }
            Type::Intersection(members) => {
                Type::Intersection(members.into_iter().map(Type::simplify_type).collect())
            }
            Type::Negation(inner) => Type::Negation(Box::new(Type::simplify_type(*inner))),
            Type::Dict(row) => {
                let fields = row
                    .fields
                    .into_iter()
                    .map(|(k, v)| (k, Type::simplify_type(v)))
                    .collect();
                Type::Dict(Row {
                    fields,
                    tail: RowTail::Empty,
                })
            }
            Type::Function {
                params,
                ret,
                variadic,
                required_count,
            } => {
                let params = params
                    .into_iter()
                    .map(|(name, ty)| (name, Type::simplify_type(ty)))
                    .collect();
                let ret = Box::new(Type::simplify_type(*ret));
                Type::Function {
                    params,
                    ret,
                    variadic,
                    required_count,
                }
            }
            Type::App(f, a) => Type::App(
                Box::new(Type::simplify_type(*f)),
                Box::new(Type::simplify_type(*a)),
            ),
            Type::TypeStageApp { fn_name, args } => Type::TypeStageApp {
                fn_name,
                args: args.into_iter().map(Type::simplify_type).collect(),
            },
            Type::NominalVariant {
                tycon,
                ctor,
                fields,
            } => {
                let simplified_fields = fields
                    .fields
                    .into_iter()
                    .map(|(k, v)| (k, Type::simplify_type(v)))
                    .collect();
                Type::NominalVariant {
                    tycon,
                    ctor,
                    fields: Row {
                        fields: simplified_fields,
                        tail: RowTail::Empty,
                    },
                }
            }
            // S-860: equirecursive-types-core — recurse into the body.
            Type::Recursive { var, body } => Type::Recursive {
                var,
                body: Box::new(Type::simplify_type(*body)),
            },
            _ => ty,
        }
    }

    /// Construct a `Function` type, computing `required_count` from `params.len()`.
    ///
    /// Use this constructor for functions with no optional parameters (all params required).
    /// All builtin functions use this constructor. For user-defined functions with `default:`
    /// annotations, `infer_fn_push_cont` in `typecheck_cek.rs` computes `required_count` directly
    /// (B-349: the fix for spurious arity errors on calls omitting optional params).
    pub fn fn_type(params: Vec<(Option<String>, Type)>, ret: Type, variadic: bool) -> Self {
        let required_count = params.len();
        Type::Function {
            params,
            ret: Box::new(ret),
            variadic,
            required_count,
        }
    }

    /// Construct `Map[k, v]` as `App(App(TyCon("Map"), k), v)` (curried).
    pub fn map(k: Type, v: Type) -> Self {
        Type::App(
            Box::new(Type::App(Box::new(Type::TyCon("Map".into())), Box::new(k))),
            Box::new(v),
        )
    }

    /// Construct `Handle[cap]` as `App(TyCon("Handle"), cap)`.
    pub fn handle(cap: Type) -> Self {
        Type::App(Box::new(Type::TyCon("Handle".into())), Box::new(cap))
    }

    /// Destructure `Map[k, v]` → `Some((k, v))` or `None`.
    pub fn as_map(&self) -> Option<(&Type, &Type)> {
        if let Type::App(fv, v) = self {
            if let Type::App(fk, k) = fv.as_ref() {
                if matches!(fk.as_ref(), Type::TyCon(n) if n == "Map") {
                    return Some((k, v));
                }
            }
        }
        None
    }

    /// Destructure `Handle[cap]` → `Some(cap)` or `None`.
    pub fn as_handle(&self) -> Option<&Type> {
        if let Type::App(f, arg) = self {
            if matches!(f.as_ref(), Type::TyCon(n) if n == "Handle") {
                return Some(arg);
            }
        }
        None
    }

    /// Check if this type is `TyCon(name)`.
    pub fn is_tycon(&self, name: &str) -> bool {
        matches!(self, Type::TyCon(n) if n == name)
    }
}

/// Helper for normalize_union: assign a stable sort order to each Type variant.
fn type_order(ty: &Type) -> u8 {
    match ty {
        Type::Int => 0,
        Type::IntLiteral(_) => 1,
        Type::Float => 2,
        Type::Str => 3,
        Type::StringLiteral(_) => 4,
        Type::Bytes => 6,
        Type::Dict(_) => 8,
        Type::Function { .. } => 9,
        Type::Proxy => 12,
        Type::TypeVar(_, _) => 13,
        Type::Unknown => 14,
        Type::Any => 15,
        Type::Error(_) => 16,
        Type::DirCap => 17,
        Type::NetCap => 18,
        Type::Uri => 20,
        Type::Timestamp => 21,
        Type::Duration => 22,
        Type::ClockCap => 23,
        Type::Timezone => 24,
        Type::QuicSession => 25,
        Type::Http2Session => 26,
        Type::Http3Session => 27,
        Type::QuicDatagramHandle => 28,
        Type::DatagramHandle => 29,
        Type::Union(_) => 30, // Should not appear after flattening, but included for completeness
        Type::Intersection(_) => 31, // Should not appear after flattening, but included for completeness
        Type::Negation(_) => 32,
        Type::Never => 33,
        Type::App(_, _) => 34,
        Type::TyCon(_) => 35,
        Type::Operator(_) => 36,
        Type::TypeStageApp { .. } => 37,
        Type::NominalVariant { .. } => 38,
        // S-860: equirecursive-types-core
        Type::Recursive { .. } => 39,
    }
}

/// Helper for normalize_union: compare payloads for types with the same variant.
pub(crate) fn type_payload_cmp(a: &Type, b: &Type) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Type::IntLiteral(n1), Type::IntLiteral(n2)) => n1.cmp(n2),
        (Type::StringLiteral(s1), Type::StringLiteral(s2)) => s1.cmp(s2),
        (Type::TypeVar(name1, _), Type::TypeVar(name2, _)) => name1.cmp(name2),
        (Type::Operator(name1), Type::Operator(name2)) => name1.cmp(name2),
        (
            Type::TypeStageApp {
                fn_name: fn1,
                args: args1,
            },
            Type::TypeStageApp {
                fn_name: fn2,
                args: args2,
            },
        ) => match fn1.cmp(fn2) {
            Ordering::Equal => {
                // Lexicographic comparison of args
                for (a1, a2) in args1.iter().zip(args2.iter()) {
                    match type_payload_cmp(a1, a2) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                }
                args1.len().cmp(&args2.len())
            }
            other => other,
        },
        // NominalVariant: compare by tycon, then ctor, then fields (via Display for simplicity)
        (
            Type::NominalVariant {
                tycon: tycon1,
                ctor: ctor1,
                ..
            },
            Type::NominalVariant {
                tycon: tycon2,
                ctor: ctor2,
                ..
            },
        ) => match tycon1.cmp(tycon2) {
            Ordering::Equal => match ctor1.cmp(ctor2) {
                Ordering::Equal => a.to_string().cmp(&b.to_string()),
                other => other,
            },
            other => other,
        },
        // For complex types (Record, Function, App), use Display representation
        // This is not ideal but ensures stability
        (Type::Dict(_), Type::Dict(_))
        | (Type::Function { .. }, Type::Function { .. })
        | (Type::App(_, _), Type::App(_, _)) => a.to_string().cmp(&b.to_string()),
        _ => Ordering::Equal,
    }
}

/// Check that a type is well-kinded with respect to the kind environment.
///
/// This implements the [KIND-LABEL-ERROR] kinding judgment from doc/whatif/completed/hkt-monads.md:
/// Label-kinded TypeVars (Kind::Label) cannot appear in positions expecting Kind::Type (e.g., as
/// the element type of a parameterized type, as function parameters/return types, or as record field types).
///
/// Returns an error if any TypeVar in the type has Kind::Label in `kind_env`.
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

pub fn check_kind_wellformed(
    ty: &Type,
    kind_env: &HashMap<String, Kind>,
    span: Span,
) -> Result<(), TypeError> {
    match ty {
        Type::TypeVar(name, _) => {
            if let Some(Kind::Label) = kind_env.get(name.as_str()) {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "kind mismatch: type variable `{name}` has kind Label but expected kind *"
                    ),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })
                .into());
            }
            Ok(())
        }
        Type::Function { params, ret, .. } => {
            for (_name, param_ty) in params {
                check_kind_wellformed(param_ty, kind_env, span.clone())?;
            }
            check_kind_wellformed(ret, kind_env, span)
        }
        Type::Dict(row) => {
            for field_ty in row.fields.values() {
                check_kind_wellformed(field_ty, kind_env, span.clone())?;
            }
            // Also check RowTail::Uniform key and value types for kind well-formedness
            if let RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    check_kind_wellformed(k, kind_env, span.clone())?;
                }
                check_kind_wellformed(value, kind_env, span.clone())?;
            }
            Ok(())
        }
        Type::Union(members) | Type::Intersection(members) => {
            for member in members {
                check_kind_wellformed(member, kind_env, span.clone())?;
            }
            Ok(())
        }
        Type::Negation(inner) => check_kind_wellformed(inner, kind_env, span),
        Type::App(func, arg) => {
            check_kind_wellformed(func, kind_env, span.clone())?;
            check_kind_wellformed(arg, kind_env, span)
        }
        Type::Operator(name) => {
            // Bare Operator in a type position (kind *) is kind-incorrect.
            // Operator variables have kind (* → *) and must be applied via Type::App.
            if let Some(Kind::Operator) = kind_env.get(name.as_str()) {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("kind mismatch: operator `{name}` has kind (* → *) but expected kind *; did you forget to apply it?"),
                    span,
                    notes: vec![], call_stack: vec![],
                }).into());
            }
            // Bare Kind::Arrow in a type position is also kind-incorrect.
            // Arrow kinds are for higher-order type constructors that must be fully applied.
            if matches!(kind_env.get(name.as_str()), Some(Kind::Arrow(_, _))) {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("kind mismatch: `{name}` is a higher-kinded type constructor but was used in a type position"),
                    span,
                    notes: vec![], call_stack: vec![],
                }).into());
            }
            // If the name is not in kind_env, let it pass (freshly introduced Operator
            // that hasn't been kind-registered yet, or will be registered later)
            Ok(())
        }
        Type::TypeStageApp { fn_name: _, args } => {
            for arg in args {
                check_kind_wellformed(arg, kind_env, span.clone())?;
            }
            Ok(())
        }
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => {
            for field_ty in fields.fields.values() {
                check_kind_wellformed(field_ty, kind_env, span.clone())?;
            }
            Ok(())
        }
        Type::TyCon(_) => Ok(()), // TyCon is always well-kinded (it's a concrete constructor)
        // All other types (Int, Str, Bool, literals, capabilities, etc.) are always well-kinded
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================================
    // T-999: Kind::Arrow unit tests (type-system-health-s841-followup sprint)
    // ============================================================================

    #[test]
    fn test_kind_arrow_arity_two() {
        // Map has kind * → (* → *), i.e., Kind::Arrow(Kind::Type, Kind::Operator).
        // arity = 1 + arity(Kind::Operator) = 1 + 1 = 2.
        let map_kind = Kind::Arrow(
            Box::new(Kind::Type),
            Box::new(Kind::Operator), // * → *
        );
        assert_eq!(map_kind.arity(), 2, "Map (* → (* → *)) should have arity 2");
    }

    #[test]
    fn test_kind_arrow_arity_triple() {
        // Hypothetical 3-arg constructor: * → * → (* → *)
        // arity = 1 + arity(* → (* → *)) = 1 + (1 + arity(* → *)) = 1 + (1 + 1) = 3
        let triple_kind = Kind::Arrow(
            Box::new(Kind::Type),
            Box::new(Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Operator))),
        );
        assert_eq!(
            triple_kind.arity(),
            3,
            "* → * → (* → *) should have arity 3"
        );
    }

    #[test]
    fn test_kind_type_arity_zero() {
        assert_eq!(Kind::Type.arity(), 0, "Kind::Type should have arity 0");
    }

    #[test]
    fn test_kind_label_arity_zero() {
        assert_eq!(Kind::Label.arity(), 0, "Kind::Label should have arity 0");
    }

    #[test]
    fn test_kind_operator_arity_one() {
        assert_eq!(
            Kind::Operator.arity(),
            1,
            "Kind::Operator should have arity 1"
        );
    }

    #[test]
    fn test_kind_arrow_is_operator_true() {
        let map_kind = Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Operator));
        assert!(
            map_kind.is_operator(),
            "Kind::Arrow should be considered an operator"
        );
    }

    #[test]
    fn test_kind_operator_is_operator_true() {
        assert!(
            Kind::Operator.is_operator(),
            "Kind::Operator should return true for is_operator()"
        );
    }

    #[test]
    fn test_kind_type_is_operator_false() {
        assert!(
            !Kind::Type.is_operator(),
            "Kind::Type should return false for is_operator()"
        );
    }

    #[test]
    fn test_kind_label_is_operator_false() {
        assert!(
            !Kind::Label.is_operator(),
            "Kind::Label should return false for is_operator()"
        );
    }

    #[test]
    fn test_kind_arrow_display() {
        let map_kind = Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Operator));
        assert_eq!(
            format!("{}", map_kind),
            "* → * → *",
            "Kind::Arrow Display should format correctly"
        );
    }

    #[test]
    fn test_kind_type_display() {
        assert_eq!(format!("{}", Kind::Type), "*");
    }

    #[test]
    fn test_kind_operator_display() {
        assert_eq!(format!("{}", Kind::Operator), "* → *");
    }

    #[test]
    fn test_kind_label_display() {
        assert_eq!(format!("{}", Kind::Label), "Label");
    }

    // ============================================================================
    // B-435: is_subtype cross-variable mu tests
    // ============================================================================

    #[test]
    fn test_is_subtype_cross_var_mu_alpha_equivalent() {
        // µa.(Int | {x: a}) <: µb.(Int | {x: b}) should be true.
        // Cross-variable mu types with structurally equivalent bodies are subtypes.
        // S-Assum inserts (a, b) into sigma; after S-Exp unfolds both sides, the
        // union members are checked. The Record member's x-field contains the full
        // Recursive type again — S-Assum fires on the re-encounter and returns true.
        let rec_a = Type::Recursive {
            var: "a".to_string(),
            body: Box::new(Type::Union(vec![
                Type::Int,
                Type::Dict(Row {
                    fields: [("x".to_string(), Type::TypeVar("a".to_string(), 0))].into(),
                    tail: RowTail::Empty,
                }),
            ])),
        };
        let rec_b = Type::Recursive {
            var: "b".to_string(),
            body: Box::new(Type::Union(vec![
                Type::Int,
                Type::Dict(Row {
                    fields: [("x".to_string(), Type::TypeVar("b".to_string(), 0))].into(),
                    tail: RowTail::Empty,
                }),
            ])),
        };
        assert!(
            Type::is_subtype(&rec_a, &rec_b, None),
            "µa.(Int | {{x: a}}) <: µb.(Int | {{x: b}}) must hold — alpha-equivalent cross-variable mu types"
        );
        // Symmetric: b <: a must also hold
        assert!(
            Type::is_subtype(&rec_b, &rec_a, None),
            "µb.(Int | {{x: b}}) <: µa.(Int | {{x: a}}) must hold — symmetric alpha-equivalence"
        );
    }

    // ============================================================================
    // B-454: is_consistent_subtype variadic-flag mismatch tests
    // ============================================================================

    #[test]
    fn test_is_consistent_subtype_variadic_not_csubtype_of_nonvariadic_same_arity() {
        // B-454: fn(Int)... ~<: fn(Int) must be false.
        // A variadic function accepts extra arguments; a non-variadic does not. With the same
        // declared param count (1 here), the flag difference must reject consistent subtyping.
        // Without the sub_v == sup_v guard this would spuriously return true.
        let variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: true,
            required_count: 1,
        };
        let non_variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        };
        assert!(
            !Type::is_consistent_subtype(&variadic, &non_variadic),
            "variadic fn(Int)... must NOT be a consistent subtype of non-variadic fn(Int)"
        );
    }

    #[test]
    fn test_is_consistent_subtype_nonvariadic_not_csubtype_of_variadic_same_arity() {
        // Symmetric: fn(Int) ~<: fn(Int)... must also be false.
        // A non-variadic function (fixed arity) is not a consistent subtype of a variadic
        // function of the same declared arity, because callers of the variadic may pass extra
        // args that the non-variadic does not accept.
        // NOTE: the special "any-function" (zero-param variadic) case is handled earlier in the
        // arm and returns true — this tests the concrete-arity non-any-function path.
        let non_variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        };
        let variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: true,
            required_count: 1,
        };
        assert!(
            !Type::is_consistent_subtype(&non_variadic, &variadic),
            "non-variadic fn(Int) must NOT be a consistent subtype of variadic fn(Int)..."
        );
    }

    #[test]
    fn test_is_consistent_subtype_variadic_reflexive() {
        // fn(Int)... ~<: fn(Int)... must hold (same flags, same arity, reflexive).
        let variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: true,
            required_count: 1,
        };
        assert!(
            Type::is_consistent_subtype(&variadic, &variadic),
            "variadic fn(Int)... must be a consistent subtype of itself (reflexive)"
        );
    }

    #[test]
    fn test_is_consistent_subtype_nonvariadic_reflexive() {
        // fn(Int) ~<: fn(Int) must hold (same flags, same arity, reflexive).
        let non_variadic = Type::Function {
            params: vec![(Some("x".to_string()), Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        };
        assert!(
            Type::is_consistent_subtype(&non_variadic, &non_variadic),
            "non-variadic fn(Int) must be a consistent subtype of itself (reflexive)"
        );
    }

    #[test]
    fn test_is_consistent_subtype_variadic_flag_mismatch_with_unknown_params() {
        // B-454: Unknown params must not rescue a variadic-flag mismatch.
        // fn(?)... ~<: fn(?) should still be false: Unknown in the param makes the param
        // positions consistent, but the call-convention difference (variadic vs fixed) is
        // structural and must not be erased by gradual types.
        let variadic_unknown = Type::Function {
            params: vec![(Some("x".to_string()), Type::Unknown)],
            ret: Box::new(Type::Unknown),
            variadic: true,
            required_count: 1,
        };
        let non_variadic_unknown = Type::Function {
            params: vec![(Some("x".to_string()), Type::Unknown)],
            ret: Box::new(Type::Unknown),
            variadic: false,
            required_count: 1,
        };
        assert!(
            !Type::is_consistent_subtype(&variadic_unknown, &non_variadic_unknown),
            "fn(?)... must NOT be a consistent subtype of fn(?): variadic flag mismatch is not erased by Unknown"
        );
    }

    #[test]
    fn test_is_subtype_mu_reflexive() {
        // µa.{x: a} <: µa.{x: a} should be true (reflexive case).
        // When both sides are the same Recursive type, PartialEq fires at (a, b) if a == b.
        let rec = Type::Recursive {
            var: "a".to_string(),
            body: Box::new(Type::Dict(Row {
                fields: [("x".to_string(), Type::TypeVar("a".to_string(), 0))].into(),
                tail: RowTail::Empty,
            })),
        };
        assert!(
            Type::is_subtype(&rec, &rec, None),
            "µa.{{x: a}} <: µa.{{x: a}} must hold — reflexive recursive type"
        );
    }

    // --- BAS-based is_subtype tests (T-1211) ---
    // These verify that the RDNF-based subtyping judgment gives correct results
    // for all standard subtyping relationships.

    #[test]
    fn test_bas_int_subtype_int() {
        assert!(Type::is_subtype(&Type::Int, &Type::Int, None));
    }

    #[test]
    fn test_bas_int_not_subtype_str() {
        assert!(!Type::is_subtype(&Type::Int, &Type::Str, None));
    }

    #[test]
    fn test_bas_int_literal_subtype_int() {
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::Int, None));
    }

    #[test]
    fn test_bas_string_literal_subtype_str() {
        assert!(Type::is_subtype(
            &Type::StringLiteral("hello".into()),
            &Type::Str,
            None
        ));
    }

    #[test]
    fn test_bas_never_subtype_of_anything() {
        assert!(Type::is_subtype(&Type::Never, &Type::Int, None));
        assert!(Type::is_subtype(&Type::Never, &Type::Str, None));
        assert!(Type::is_subtype(&Type::Never, &Type::Any, None));
    }

    #[test]
    fn test_bas_anything_subtype_of_top() {
        assert!(Type::is_subtype(&Type::Int, &Type::Any, None));
        assert!(Type::is_subtype(&Type::Str, &Type::Any, None));
        assert!(Type::is_subtype(&Type::Never, &Type::Any, None));
    }

    #[test]
    fn test_bas_error_not_subtype() {
        assert!(!Type::is_subtype(
            &Type::error_note("test error sentinel"),
            &Type::Int,
            None
        ));
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::error_note("test error sentinel"),
            None
        ));
    }

    #[test]
    fn test_bas_unknown_not_subtype() {
        assert!(!Type::is_subtype(&Type::Unknown, &Type::Int, None));
        assert!(!Type::is_subtype(&Type::Int, &Type::Unknown, None));
    }

    #[test]
    fn test_bas_typevar_subtype() {
        // TypeVar returns true (conservative approximation — defers to constraint solver).
        // The TypeVar guard in is_subtype_bas fires before RDNF, so any TypeVar in
        // either position returns true without inspecting the other side.
        assert!(Type::is_subtype(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            None
        ));
        assert!(Type::is_subtype(
            &Type::Int,
            &Type::TypeVar("a".into(), 0),
            None
        ));
    }

    /// B-446: TypeVar guard in is_subtype_bas returns true unconditionally for ANY TypeVar.
    ///
    /// This test documents the exact approximation semantics:
    ///
    /// 1. `TypeVar("a") <: TypeVar("b")` is true — even for DIFFERENT variables.
    ///    This does NOT mean any concrete instantiation of `a` is a subtype of any
    ///    concrete instantiation of `b`. The guard is a deferral, not a proof.
    ///
    /// 2. Transitivity is NOT preserved across the TypeVar guard:
    ///    `TypeVar("a") <: Int` = true  AND  `TypeVar("b") <: Str` = true
    ///    does NOT imply any relationship between `a` and `b`.
    ///    The constraint solver (constrain(), unify()) enforces actual bounds.
    ///
    /// 3. Callers that need a precise judgment must apply the substitution first
    ///    and call is_subtype on the resolved (ground) types.
    ///
    /// 4. Contrast with Unknown: Unknown returns FALSE from is_subtype (it uses
    ///    is_consistent instead). TypeVar differs because it is an inference variable
    ///    expected to be solved; Unknown is the deliberate gradual "?" type.
    #[test]
    fn test_bas_typevar_subtype_b446_approximation_semantics() {
        // Two DIFFERENT TypeVars: returns true (deferral, not a proof of subtyping).
        assert!(
            Type::is_subtype(
                &Type::TypeVar("a".into(), 0),
                &Type::TypeVar("b".into(), 0),
                None
            ),
            "TypeVar(a) <: TypeVar(b) must be true (conservative approximation)"
        );

        // Same TypeVar on both sides: also true (subsumes the reflexivity short-circuit).
        assert!(
            Type::is_subtype(
                &Type::TypeVar("a".into(), 0),
                &Type::TypeVar("a".into(), 0),
                None
            ),
            "TypeVar(a) <: TypeVar(a) must be true (reflexive)"
        );

        // TypeVar vs Never (guard order: Error, S-TOP, S-NEVER, Unknown, TypeVar):
        // - TypeVar sub, Never sup: Error=no, S-TOP=no, S-NEVER(sub=TypeVar)=no,
        //   Unknown=no, TypeVar(sub is TypeVar) → true.
        //   NOTE: this is a known artifact of the approximation. In proper type theory
        //   only Never <: Never holds (Nothing is a subtype of Bottom except Bottom).
        //   The TypeVar guard defers this to the constraint solver.
        assert!(
            Type::is_subtype(&Type::TypeVar("a".into(), 0), &Type::Never, None),
            "TypeVar(a) <: Never is true per the approximation guard (see B-446)"
        );
        // Never sub, TypeVar sup: S-NEVER fires first (sub=Never) → true.
        assert!(
            Type::is_subtype(&Type::Never, &Type::TypeVar("a".into(), 0), None),
            "Never <: TypeVar(a) must be true (S-NEVER fires first, correct)"
        );

        // TypeVar vs Error: Error guard fires first → false on BOTH sides.
        assert!(
            !Type::is_subtype(
                &Type::TypeVar("a".into(), 0),
                &Type::error_note("test error sentinel"),
                None
            ),
            "TypeVar(a) <: Error must be false (Error guard fires first)"
        );
        assert!(
            !Type::is_subtype(
                &Type::error_note("test error sentinel"),
                &Type::TypeVar("a".into(), 0),
                None
            ),
            "Error <: TypeVar(a) must be false (Error guard fires first)"
        );

        // TypeVar vs Unknown: Unknown guard fires first → false on BOTH sides.
        // Unknown is not in the subtype lattice; only is_consistent handles it.
        assert!(
            !Type::is_subtype(&Type::TypeVar("a".into(), 0), &Type::Unknown, None),
            "TypeVar(a) <: Unknown must be false (Unknown guard fires first)"
        );
        assert!(
            !Type::is_subtype(&Type::Unknown, &Type::TypeVar("a".into(), 0), None),
            "Unknown <: TypeVar(a) must be false (Unknown guard fires first)"
        );
    }

    #[test]
    fn test_bas_record_width_subtyping() {
        // {x: Int, y: Str} <: {x: Int} — width subtyping
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m.insert("y".into(), Type::Str);
                m
            },
            tail: RowTail::Empty,
        });
        let sup = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m
            },
            tail: RowTail::Empty,
        });
        assert!(Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_record_width_not_reverse() {
        // {x: Int} NOT <: {x: Int, y: Str} — missing field y
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m
            },
            tail: RowTail::Empty,
        });
        let sup = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m.insert("y".into(), Type::Str);
                m
            },
            tail: RowTail::Empty,
        });
        assert!(!Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_record_field_depth_subtyping() {
        // {x: IntLiteral(42)} <: {x: Int} — depth subtyping on field value
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::IntLiteral(42));
                m
            },
            tail: RowTail::Empty,
        });
        let sup = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m
            },
            tail: RowTail::Empty,
        });
        assert!(Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_multifield_record_depth_mismatch_not_subtype() {
        // {x:Int, y:Str} NOT <: {x:Int, y:Float} — field y has mismatched type.
        // Regression test for the F1 soundness fix: atoms_are_disjoint previously
        // returned true for {x:T} vs {y:U} (different keys), which made the conjunction
        // [Pos({x:Int}), Pos({y:Str}), Neg({x:Int}), Neg({y:Float})] appear empty via
        // disjointness before the subtype check fired, incorrectly returning true.
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m.insert("y".into(), Type::Str);
                m
            },
            tail: RowTail::Empty,
        });
        let sup = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m.insert("y".into(), Type::Float);
                m
            },
            tail: RowTail::Empty,
        });
        assert!(!Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_union_subtype() {
        // Int <: Int | Str
        assert!(Type::is_subtype(
            &Type::Int,
            &Type::Union(vec![Type::Int, Type::Str]),
            None
        ));
    }

    #[test]
    fn test_bas_union_elim() {
        // Int | Str <: Int | Str | Float
        assert!(Type::is_subtype(
            &Type::Union(vec![Type::Int, Type::Str]),
            &Type::Union(vec![Type::Int, Type::Str, Type::Float]),
            None
        ));
    }

    #[test]
    fn test_bas_union_not_subtype_of_member() {
        // Int | Str NOT <: Int — Str values don't satisfy Int
        assert!(!Type::is_subtype(
            &Type::Union(vec![Type::Int, Type::Str]),
            &Type::Int,
            None
        ));
    }

    #[test]
    fn test_bas_negation_int_subtype_not_str() {
        // Int <: ~Str — Int values are not Str
        assert!(Type::is_subtype(
            &Type::Int,
            &Type::Negation(Box::new(Type::Str)),
            None
        ));
    }

    #[test]
    fn test_bas_negation_int_not_subtype_not_int() {
        // Int NOT <: ~Int — Int values ARE Int
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::Negation(Box::new(Type::Int)),
            None
        ));
    }

    #[test]
    fn test_bas_negation_contravariant() {
        // ~Str <: ~IntLiteral(42)? Iff IntLiteral(42) <: Str, which is false.
        assert!(!Type::is_subtype(
            &Type::Negation(Box::new(Type::Str)),
            &Type::Negation(Box::new(Type::IntLiteral(42))),
            None
        ));
        // ~Int <: ~IntLiteral(42)? Iff IntLiteral(42) <: Int, which is true.
        assert!(Type::is_subtype(
            &Type::Negation(Box::new(Type::Int)),
            &Type::Negation(Box::new(Type::IntLiteral(42))),
            None
        ));
    }

    #[test]
    fn test_bas_intersection_subtype() {
        // Int & ~Str <: Int (intersection is subtype of any member)
        assert!(Type::is_subtype(
            &Type::Intersection(vec![Type::Int, Type::Negation(Box::new(Type::Str))]),
            &Type::Int,
            None
        ));
    }

    #[test]
    fn test_bas_function_subtype() {
        // Function(Int -> Str) <: Function(Int -> Str) — reflexive
        let f = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        };
        assert!(Type::is_subtype(&f, &f, None));
    }

    #[test]
    fn test_bas_function_contravariant_params() {
        // Function(Int -> Str) <: Function(IntLiteral(42) -> Str)
        // Contravariant params: sup_param <: sub_param, i.e., IntLiteral(42) <: Int. True.
        let sub = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        };
        let sup = Type::Function {
            params: vec![(None, Type::IntLiteral(42))],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        };
        assert!(Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_function_covariant_return() {
        // Function(Int -> IntLiteral(42)) <: Function(Int -> Int)
        let sub = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::IntLiteral(42)),
            variadic: false,
            required_count: 1,
        };
        let sup = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        };
        assert!(Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_nominal_variant_same_tag() {
        let sub = Type::NominalVariant {
            tycon: "Result".into(),
            ctor: "Ok".into(),
            fields: Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("value".into(), Type::IntLiteral(42));
                    m
                },
                tail: RowTail::Empty,
            },
        };
        let sup = Type::NominalVariant {
            tycon: "Result".into(),
            ctor: "Ok".into(),
            fields: Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("value".into(), Type::Int);
                    m
                },
                tail: RowTail::Empty,
            },
        };
        assert!(Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_nominal_variant_different_tags() {
        let sub = Type::NominalVariant {
            tycon: "Result".into(),
            ctor: "Ok".into(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        let sup = Type::NominalVariant {
            tycon: "Result".into(),
            ctor: "Err".into(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        assert!(!Type::is_subtype(&sub, &sup, None));
    }

    #[test]
    fn test_bas_s_rcd_top() {
        // Union of closed single-field records with different keys = Top
        // {x: Int} | {y: Str} should be treated as Top
        // So: {x: Int} | {y: Str} <: Top → true
        // And: Top <: {x: Int} | {y: Str} → true (they're equivalent)
        let union = Type::Union(vec![
            Type::Dict(Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("x".into(), Type::Int);
                    m
                },
                tail: RowTail::Empty,
            }),
            Type::Dict(Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("y".into(), Type::Str);
                    m
                },
                tail: RowTail::Empty,
            }),
        ]);
        // The union includes all possible single-field records at x and y
        // Under BAS, {x: T} | {y: U} = Top when these are the ONLY record shapes
        // Note: This test verifies BAS handles S-RcdTop correctly
        assert!(Type::is_subtype(&union, &Type::Any, None));
    }

    #[test]
    fn test_bas_app_covariant() {
        // App(TyCon, Int) <: App(TyCon, Int) with tycon env
        use std::sync::Arc;
        let tycon_env = {
            let mut env = HashMap::new();
            env.insert(
                "Seq".to_string(),
                Arc::new(TyConDef {
                    params: vec!["a".to_string()],
                    body: Type::Unknown,
                    constraints: vec![],
                    variance: vec![Variance::Covariant],
                    constructors: vec![],
                    builtin_type: Some("Seq".to_string()),
                    annotation: None,
                    field_annotations: IndexMap::new(),
                    constructor_constants: IndexMap::new(),
                    definition_span: None,
                }),
            );
            env
        };
        let coll_int = Type::App(Box::new(Type::TyCon("Coll".into())), Box::new(Type::Int));
        assert!(Type::is_subtype(&coll_int, &coll_int, Some(&tycon_env)));
    }

    #[test]
    fn test_bas_false_branch_narrowing() {
        // (Int | Str) & ~Int = Str  (BAS false-branch narrowing)
        // So: (Int | Str) & ~Int <: Str should be true
        let narrowed = Type::Intersection(vec![
            Type::Union(vec![Type::Int, Type::Str]),
            Type::Negation(Box::new(Type::Int)),
        ]);
        assert!(Type::is_subtype(&narrowed, &Type::Str, None));
    }

    #[test]
    fn test_bas_record_uniform_tail_subtype() {
        // {x: Int, Uniform(None, Int)} <: {Uniform(None, Int)}
        // Record with named fields + uniform tail is subtype of just the uniform tail
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".into(), Type::Int);
                m
            },
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Int),
            },
        });
        let sup = Type::Dict(Row {
            fields: IndexMap::new(),
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Int),
            },
        });
        assert!(Type::is_subtype(&sub, &sup, None));
    }
}
