//! Core type representations for the LLT type system.
//!
//! This module contains the `Type` enum, `Row` struct for record types, kind definitions,
//! and purely structural operations on types (subtyping, consistency, variable collection).
//!
//! Inference machinery (`InferState`, generalization) lives in `type_infer.rs`.
//! Substitution and unify live in `type_unify.rs`.
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
/// `fields` uses `HashMap` because row field order is semantically irrelevant at the type level —
/// structural subtyping makes rows unordered. `Display` sorts field names for
/// deterministic output. Runtime `Value::Dict` keeps `IndexMap` for ordered user-visible
/// semantics; this HashMap is only at the type-inference layer.
///
/// `tail` constrains the non-named portion of the row. `RowTail::Empty` is the default for
/// all current closed-record constructions. `RowTail::Uniform` is produced when parsing
/// `{_ : V}` or `{_@K : V}` annotation syntax (column constraints).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub fields: HashMap<String, Type>, // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,
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
    /// Migrated from TypeAlias.params (T-1064).
    pub params: Vec<String>,

    /// Type body. For structural aliases, this is the expanded type; for nominal ADTs,
    /// this is typically a Union of NominalVariants. Migrated from TypeAlias.body (T-1064).
    pub body: Type,

    /// Class constraints on type parameters, populated when params carry `@ClassName` annotations.
    /// Empty for unconstrained aliases. Migrated from TypeAlias.constraints (T-1064).
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
}

impl TyConDef {
    pub fn arity(&self) -> usize {
        self.params.len()
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
    Bool,
    Bytes,
    /// Supertype of both `Int` and `Float` — represents any numeric value.
    /// No `NumberLiteral` variant exists (unlike `IntLiteral`/`StringLiteral`) because:
    /// - Literals parse to concrete types (`IntLiteral` or `Float`)
    /// - `Number` only appears in user annotations (`[@Number ...]`) and subtyping relations
    /// - There is no runtime value that is "a number but neither int nor float"
    Number,
    Record(Row),
    Function {
        params: Vec<(Option<String>, Type)>, // (param_name, param_type) — None = positional-only
        ret: Box<Type>,
        variadic: bool,
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
    /// Top type — ⊤, the true supertype of everything. Represents "any type is allowed here"
    /// (TypeAssert upper bound, explicit "accept anything" positions).
    /// All types τ satisfy τ <: Top.
    Top,
    /// Sentinel for failed sub-expression inference. Prevents cascade errors: when a
    /// sub-expression fails type inference, its result is `Error` rather than propagating
    /// the failure to parent expressions. `unify(Error, T)` is a no-op for all T (silent
    /// absorption), so parent expressions can continue inference without spurious downstream
    /// errors. `is_subtype(Error, _)` returns false; Error is not a subtype of anything.
    Error,
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
    /// Example: after `[int? x]` fails, x : (Int | Str) & ~Int = Str.
    /// In annotation syntax: @[[without A]].
    Negation(Box<Type>),
    /// Never type — ⊥, the bottom type. Represents "no value can inhabit this type".
    /// The empty intersection. Intersections that simplify to Never (e.g., Int & Str,
    /// #Ok & #Err via S-ClsBot) become Never. In annotation syntax: @Never.
    Never,
    /// Type constructor application — `App(f, a)` represents type constructor `f` applied to type `a`.
    /// Example: `App(TyCon("Seq"), Int)` for a sequence of integers.
    /// Example: `App(App(TyCon("Map"), Str), Int)` for Map[Str, Int] (curried).
    App(Box<Type>, Box<Type>),
    /// Named type constructor — a concrete type constructor like `Seq`, `Map`, or `Handle`.
    /// Used as the head of `App` chains: `App(TyCon("Seq"), Int)` = Seq[Int].
    /// Display: just the name (e.g., `Seq`).
    TyCon(String),
    /// Type constructor variable — represents a type constructor like `m` in `Monad m`.
    /// Kind: `Operator` (i.e., `* → *`). Used in typeclass constraints and generic functions.
    Operator(String),
    /// Type-stage function application — represents a pending type-level computation.
    /// Created during constraint generation for FD classes; reduced by normalize().
    /// Example: TypeStageApp { fn_name: "AddResult", args: vec![Int, Float] } reduces to Float.
    #[allow(clippy::enum_variant_names)]
    // Type prefix is intentional for type-level computation
    TypeStageApp {
        fn_name: String,
        args: Vec<Type>,
    },
    /// Nominal variant — a union member that carries its declared constructor name.
    /// Used for nominal variants like `[Some a]`, `[IntLiteral value: Int span: AstSpan]`, and `None`.
    /// The `tag` is the constructor name (e.g., "Some", "IntLiteral", "None"), and `fields` are
    /// the named or positional payload fields.
    /// Distinct from structural `Record` types — nominal variants are never subtypes of records.
    NominalVariant {
        tag: String,
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
            (Type::Bool, Type::Bool) => true,
            (Type::Bytes, Type::Bytes) => true,
            (Type::Number, Type::Number) => true,
            (Type::Record(row1), Type::Record(row2)) => row1 == row2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                    variadic: v1,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                    variadic: v2,
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
            (Type::Top, Type::Top) => true,
            (Type::Error, Type::Error) => true,
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
                    tag: tag1,
                    fields: fields1,
                },
                Type::NominalVariant {
                    tag: tag2,
                    fields: fields2,
                },
            ) => tag1 == tag2 && fields1 == fields2,
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
            | Type::Bool
            | Type::Bytes
            | Type::Number
            | Type::Proxy
            | Type::Unknown
            | Type::Top
            | Type::Error
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
            Type::IntLiteral(v) => v.hash(state),
            Type::StringLiteral(s) => s.hash(state),
            Type::Record(row) => {
                // Hash fields in sorted order for deterministic hashing
                let mut fields: Vec<_> = row.fields.iter().collect();
                fields.sort_by_key(|(k, _)| *k);
                fields.hash(state);
                row.tail.hash(state);
            }
            Type::Function {
                params,
                ret,
                variadic,
            } => {
                // Hash parameter types (ignore names for equality)
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
            Type::NominalVariant { tag, fields } => {
                tag.hash(state);
                // Hash fields in sorted order for deterministic hashing
                let mut field_vec: Vec<_> = fields.fields.iter().collect();
                field_vec.sort_by_key(|(k, _)| *k);
                field_vec.hash(state);
                fields.tail.hash(state);
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

/// Maximum recursion depth for subtype checking.
/// Prevents stack overflow on pathological recursive types (defense-in-depth).
const MAX_SUBTYPE_DEPTH: usize = 256;

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
        Type::Record(row) => Type::Record(substitute_recvar_row(row, var_name, replacement)),
        Type::Function {
            params,
            ret,
            variadic,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, ty)| (name.clone(), substitute_recvar(ty, var_name, replacement)))
                .collect(),
            ret: Box::new(substitute_recvar(ret, var_name, replacement)),
            variadic: *variadic,
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
        Type::NominalVariant { tag, fields } => Type::NominalVariant {
            tag: tag.clone(),
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
/// type again. When `is_subtype_inner` encounters those positions, S-Assum fires
/// immediately — the hypothesis `(v1, v2)` is already in sigma.
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
/// `App(TyCon("Seq"), T)`         → `Some(("Seq", [&T]))`
/// `TyCon("Foo")`                 → `Some(("Foo", []))`  (zero-arity)
/// Any other form                 → `None`
///
/// Arguments are returned in application order (left-to-right): the leftmost parameter of
/// the original `[type Foo a b]` declaration is `args[0]`, the rightmost is `args[n-1]`.
fn extract_tycon_spine(ty: &Type) -> Option<(&str, Vec<&Type>)> {
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
    /// S-861: equirecursive-checker — allocates the coinductive sigma context (Chau & Parreaux
    /// 2026, S-Exp + S-Assum) once per top-level call and threads it through all recursive
    /// calls via `is_subtype_inner`. The sigma set records `(a.var, b.var)` pairs for
    /// `Recursive` types already under comparison — S-Assum fires (returns `true`) when the
    /// same pair is encountered again, preventing divergence on cyclic types.
    pub fn is_subtype(
        sub: &Type,
        sup: &Type,
        tycon_env: Option<&crate::type_def::TyConEnv>,
    ) -> bool {
        // S-861: equirecursive-checker — allocate sigma once; threaded through all sub-calls.
        let mut sigma: HashSet<(String, String)> = HashSet::new();
        Self::is_subtype_inner(sub, sup, tycon_env, 0, &mut sigma)
    }

    /// Recursive worker for `is_subtype`.
    ///
    /// `sigma` is the coinductive hypothesis set: `(a.var, b.var)` pairs for
    /// `Type::Recursive` types currently under comparison (S-Exp + S-Assum, Chau &
    /// Parreaux 2026). Every recursive call MUST pass `sigma` — the Rust borrow checker
    /// enforces this structurally (missing `sigma` is a compile error).
    ///
    /// ## Sigma representation: `HashSet<(String, String)>`
    ///
    /// Sigma stores pairs of μ-binder names (e.g. `"𝜇ꜱʏᴍ⧼IntList⧽42"`).
    /// `HashSet<(String, String)>` is the correct representation here for two reasons:
    ///
    /// 1. **Short-lived**: sigma is allocated once per top-level `is_subtype` call and
    ///    dropped immediately after. It does not persist between calls, so there is no
    ///    cross-call sharing that would benefit from interning.
    /// 2. **O(depth) entries**: recursive types in practice have shallow depth (bounded by
    ///    `MAX_SUBTYPE_DEPTH`). The set is never large — O(depth) entries at most, where
    ///    depth is the nesting level of μ-binders. `String` cloning at insertion is not a
    ///    bottleneck.
    ///
    /// `Arc<str>` interning would only help if the same binder names were looked up across
    /// many long-lived sigma sets — not the case here.
    ///
    /// TODO(T-1167): if recursive types become common after S-862 migration and profiling
    /// shows sigma allocation in hot paths, consider interning var names via `Arc<str>`.
    // S-861: equirecursive-checker
    fn is_subtype_inner(
        sub: &Type,
        sup: &Type,
        tycon_env: Option<&crate::type_def::TyConEnv>,
        depth: usize,
        sigma: &mut HashSet<(String, String)>,
    ) -> bool {
        // Depth guard: prevent unbounded recursion on pathological recursive types
        if depth >= MAX_SUBTYPE_DEPTH {
            return false;
        }

        // S-861: equirecursive-checker — S-Assum + S-Exp (Chau & Parreaux 2026 §3.3.1).
        //
        // These arms must come BEFORE Error/Top/Never/Unknown guards: a `Recursive` type
        // is not Error/Top/Never/Unknown, so the guards would pass through — but placing
        // S-Assum first makes the structure explicit and avoids any ordering surprises as
        // new early guards are added in future sprints.
        //
        // [S-Assum]: if both sides are Recursive and (sub.var, sup.var) ∈ sigma,
        // return `true` immediately (coinductive hypothesis).  Insert the pair before
        // continuing so that sub-checks triggered by S-Exp can use it.
        if let (Type::Recursive { var: v1, .. }, Type::Recursive { var: v2, .. }) = (sub, sup) {
            let key = (v1.clone(), v2.clone());
            if sigma.contains(&key) {
                return true;
            }
            sigma.insert(key);
            // Both sides are Recursive: unfold both and recurse.  The hypothesis is now in
            // sigma, so any re-encounter of (v1, v2) terminates via S-Assum above.
            // [S-Exp left + right]
            return Self::is_subtype_inner(
                &unfold_once(sub),
                &unfold_once(sup),
                tycon_env,
                depth + 1,
                sigma,
            );
        }
        // [S-Exp left]: only sub is Recursive; sup is concrete — unfold sub once and recurse.
        // Termination: guaranteed by the depth guard and structural induction on the concrete
        // sup (each recursive call either makes progress matching a field of sup, or fails).
        // sigma does not independently protect the asymmetric case — it only contains pairs
        // inserted by the symmetric S-Assum arm (where both sides were Recursive). If
        // S-Assum previously inserted (sub.var, _) from an ancestor call, it remains in
        // sigma but plays no role here; the depth guard is the actual backstop.
        if matches!(sub, Type::Recursive { .. }) {
            return Self::is_subtype_inner(&unfold_once(sub), sup, tycon_env, depth + 1, sigma);
        }
        // [S-Exp right]: only sup is Recursive — unfold sup once and recurse.
        if matches!(sup, Type::Recursive { .. }) {
            return Self::is_subtype_inner(sub, &unfold_once(sup), tycon_env, depth + 1, sigma);
        }

        // Error is not a subtype of anything (not even itself), and nothing is a subtype of Error.
        // It is a sentinel for failed inference and should not satisfy any constraint.
        if matches!(sub, Type::Error) || matches!(sup, Type::Error) {
            return false;
        }
        // [S-TOP]: τ <: Top for all τ (Top is the supertype of everything)
        if matches!(sup, Type::Top) {
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

        // S-861: equirecursive-checker — TypeVar on either side → true (gradual typing).
        // An unresolved TypeVar represents an inference variable that will be constrained
        // elsewhere (unlike `Unknown`, which is the gradual `?` type and returns `false`
        // from `is_subtype`). Returning `true` defers rejection to runtime or to the
        // unification constraint solver — consistent with tinct's gradual typing guarantee.
        // This fires only after S-Exp, so TypeVar is never mistaken for a Recursive binder.
        if matches!(sub, Type::TypeVar(_, _)) || matches!(sup, Type::TypeVar(_, _)) {
            return true;
        }

        match (sub, sup) {
            (a, b) if a == b => true,
            // App(f1, a1) <: App(f2, a2): variance-directed via TyConEnv when available.
            // TyCon("Seq") App: Seq[A] <: Seq[B] when A <: B (covariant).
            // TyCon("Map") App: Map[K,V1] <: Map[K,V2] when V1 <: V2 (K invariant).
            //
            // Curried multi-parameter types: App(App(TyCon("Map"),K),V) must walk the full
            // spine to find the root TyCon and look up variance for EACH parameter position.
            // Falling through to structural recursion would treat every outer-App argument as
            // covariant regardless of the declared variance — incorrect for invariant params.
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                // sub and sup are the original App types — pass them directly to avoid
                // cloning. The destructured f1/a1/f2/a2 are still used in the fallback below.
                if let (Some((name1, args1)), Some((name2, args2))) =
                    (extract_tycon_spine(sub), extract_tycon_spine(sup))
                {
                    if name1 == name2 && args1.len() == args2.len() {
                        // Both are curried applications of the same TyCon.
                        if let Some(env) = tycon_env {
                            if let Some(def) = env.get(name1) {
                                // Check each argument position using its declared variance.
                                for (i, (sub_arg, sup_arg)) in
                                    args1.iter().zip(args2.iter()).enumerate()
                                {
                                    let var =
                                        def.variance.get(i).copied().unwrap_or(Variance::Invariant);
                                    let ok = match var {
                                        Variance::Covariant => Self::is_subtype_inner(
                                            sub_arg,
                                            sup_arg,
                                            tycon_env,
                                            depth + 1,
                                            sigma,
                                        ),
                                        Variance::Contravariant => Self::is_subtype_inner(
                                            sup_arg,
                                            sub_arg,
                                            tycon_env,
                                            depth + 1,
                                            sigma,
                                        ),
                                        Variance::Invariant => sub_arg == sup_arg,
                                        Variance::Phantom => true,
                                    };
                                    if !ok {
                                        return false;
                                    }
                                }
                                return true;
                            }
                        }
                        // No env or no def: conservative invariant fallback for all positions.
                        return args1.iter().zip(args2.iter()).all(|(a, b)| a == b);
                    }
                }
                // Different TyCons, or non-TyCon App (e.g., type-function application):
                // recurse structurally on both components.
                Self::is_subtype_inner(f1, f2, tycon_env, depth + 1, sigma)
                    && Self::is_subtype_inner(a1, a2, tycon_env, depth + 1, sigma)
            }
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::Str) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            // [UNION-INJ-L] and [UNION-INJ-R]: any member is a subtype of the union
            (sub_ty, Type::Union(sup_members)) => sup_members
                .iter()
                .any(|member| Self::is_subtype_inner(sub_ty, member, tycon_env, depth + 1, sigma)),
            // [S-RcdTop] (BAS width subtyping): A union of closed single-field records with
            // disjoint field names is equivalent to Top in the BAS lattice.  The union
            // `{x: τ} | {y: π}` cannot be refined further by structural subtyping — together
            // these two shapes cover the entire closed-record universe at those labels.
            // Since Top <: T holds only when T = Top (already handled by the S-TOP guard
            // above), this fires as a pass-through to the S-TOP result when sup is Top, and
            // correctly returns false for any non-Top supertype.
            (Type::Union(sub_members), sup_ty) if Self::check_s_rcd_top(sub_members).is_some() => {
                // The union is semantically Top; delegate to is_subtype(Top, sup_ty).
                // S-TOP (sup == Top) is already handled before the match, so we only
                // reach here when sup is NOT Top — meaning Top is not a subtype of it.
                matches!(sup_ty, Type::Top)
            }
            // [UNION-ELIM]: union is a subtype iff ALL members are subtypes
            (Type::Union(sub_members), sup_ty) => sub_members
                .iter()
                .all(|member| Self::is_subtype_inner(member, sup_ty, tycon_env, depth + 1, sigma)),
            // [S-ClsBot] (nominal disjointness / structural annihilation): An intersection of
            // two or more closed single-field records with DIFFERENT field names is uninhabited
            // — no value can simultaneously be `{x: τ}` (exactly field x) and `{y: π}`
            // (exactly field y) when x ≠ y.  This is the structural analogue of S-ClsBot
            // (#C1 & #C2 ≤ Never) for nominal class tags.  Since the intersection reduces to
            // Never, and Never <: T for all T [S-NEVER], we return true.
            (Type::Intersection(sub_members), _sup_ty) if Self::check_s_cls_bot(sub_members) => {
                true // intersection ≡ Never, and Never <: anything [S-NEVER]
            }
            // [INTERSECT-INTRO]: intersection is a subtype of any of its members
            (Type::Intersection(sub_members), sup_ty) => sub_members
                .iter()
                .any(|member| Self::is_subtype_inner(member, sup_ty, tycon_env, depth + 1, sigma)),
            // [INTERSECT-ELIM]: type is a subtype of intersection iff it's a subtype of ALL members
            (sub_ty, Type::Intersection(sup_members)) => sup_members
                .iter()
                .all(|member| Self::is_subtype_inner(sub_ty, member, tycon_env, depth + 1, sigma)),
            // Negation: A <: ~B iff A and B are disjoint (for now, conservative: only reflexive negation)
            // Full BAS subtyping requires RDNF normalization — this is a placeholder
            (Type::Negation(t1), Type::Negation(t2)) => {
                Self::is_subtype_inner(t2, t1, tycon_env, depth + 1, sigma) // contravariant
            }
            // Negation subtyping: T <: ~A iff T and A are disjoint (no values in common).
            // Full BAS uses RDNF normalization to compute T ∩ A = Never, but we use a
            // conservative syntactic disjointness check that catches obvious cases like
            // Int <: ~String (true) and Int <: ~Int (false).
            (sub_ty, Type::Negation(a)) => Type::types_are_disjoint(sub_ty, a),
            // Note: Handle[cap] is now App(TyCon("Handle"), cap); handled by the App arm above.
            // Capability types: reflexive only (DirCap <: DirCap, etc.)
            // The equality check at the top of the match handles this, but we document it here.
            // All capability types are subtypes of Any (handled by Any short-circuit above).
            (Type::Record(sub_row), Type::Record(sup_row)) => {
                // BAS width subtyping:
                //
                // R1 <: R2 iff all keys of R2 are in R1 with compatible types.
                // Extra fields in R1 beyond those in R2 are always allowed (conjunction
                // elimination: a record satisfies an annotation if it has AT LEAST those fields).
                //
                // The only case that fails is when a required field of R2 is missing from R1.
                for (k, sup_ty) in &sup_row.fields {
                    match sub_row.fields.get(k) {
                        Some(sub_ty) => {
                            if !Self::is_subtype_inner(sub_ty, sup_ty, tycon_env, depth + 1, sigma)
                            {
                                return false;
                            }
                        }
                        None => {
                            // Required field k is absent from sub's known fields.
                            // Whether R1 is open or closed, we cannot prove R1 has field k
                            // without it being in the known field set. Reject.
                            return false;
                        }
                    }
                }

                // Tail subtyping rules for RowTail::Uniform:
                //
                // [S-ROW-CLOSED-TO-UNIFORM]: {f1:T1, ..., fn:Tn, Empty} <: {Uniform(None, V)}
                //     when all Ti <: V
                // [S-UNIFORM-COV]: {Uniform(None, V1)} <: {Uniform(None, V2)}
                //     when V1 <: V2  (covariant in value)
                // [S-MIXED-TO-UNIFORM]: {fi:Ti, Uniform(None, V1)} <: {Uniform(None, V2)}
                //     when Ti <: V2 and V1 <: V2
                // [S-TYPED-KEY-UNIFORM]: {Uniform(Some(K1), V1)} <: {Uniform(Some(K2), V2)}
                //     when K1 <: K2 and V1 <: V2
                // [S-KEYED-TO-UNKEYED]: {Uniform(Some(K), V)} <: {Uniform(None, V)}  always
                match (&sub_row.tail, &sup_row.tail) {
                    // sub has Empty tail, sup has Empty tail — allowed (width subtyping above satisfied)
                    (RowTail::Empty, RowTail::Empty) => {}
                    // sub is closed/empty, sup has Uniform constraint — all sub fields must satisfy V
                    // [S-ROW-CLOSED-TO-UNIFORM] and [S-MIXED-TO-UNIFORM]
                    (
                        sub_tail,
                        RowTail::Uniform {
                            key: sup_key,
                            value: sup_v,
                        },
                    ) => {
                        // sub's named fields must all be subtypes of sup_v
                        for sub_field_ty in sub_row.fields.values() {
                            if !Self::is_subtype_inner(
                                sub_field_ty,
                                sup_v,
                                tycon_env,
                                depth + 1,
                                sigma,
                            ) {
                                return false;
                            }
                        }
                        // sub's own Uniform value type must also satisfy sup_v
                        if let RowTail::Uniform {
                            key: sub_key,
                            value: sub_v,
                        } = sub_tail
                        {
                            if !Self::is_subtype_inner(sub_v, sup_v, tycon_env, depth + 1, sigma) {
                                return false;
                            }
                            // Key compatibility: if sup has a key constraint, sub must have one too
                            // [S-TYPED-KEY-UNIFORM]: sub key <: sup key
                            // [S-KEYED-TO-UNKEYED]: if sup has no key constraint, any sub key is fine
                            if let Some(sup_k) = sup_key {
                                match sub_key {
                                    Some(sub_k) => {
                                        if !Self::is_subtype_inner(
                                            sub_k,
                                            sup_k,
                                            tycon_env,
                                            depth + 1,
                                            sigma,
                                        ) {
                                            return false;
                                        }
                                    }
                                    None => {
                                        // sup requires a key type constraint but sub has none — reject
                                        return false;
                                    }
                                }
                            }
                            // sup has no key constraint (None) — sub's key constraint is fine regardless
                        }
                        // sub is Empty (closed) with a Uniform sup — fine if all fields satisfy V (done above)
                    }
                    // sub has Uniform tail but sup is Empty — sub can have extra fields sup doesn't know about
                    // This is never a subtype: {_: V} might have additional fields beyond what Empty allows.
                    (RowTail::Uniform { .. }, RowTail::Empty) => {
                        // A Uniform-tailed sub cannot be proven to be a subtype of a closed sup.
                        // The Uniform tail means sub may have additional fields; sup (Empty) does not allow them.
                        return false;
                    }
                }

                true
            }
            (
                Type::Function {
                    params: sub_p,
                    ret: sub_r,
                    variadic: sv,
                },
                Type::Function {
                    params: sup_p,
                    ret: sup_r,
                    variadic: pv,
                },
            ) => {
                // Special case: zero-param variadic is the "any function" type.
                // Function{params:[], ret:Unknown, variadic:true} is a supertype of functions
                // with concrete arity (at least one param). It is NOT a supertype of zero-param
                // non-variadic (different semantics).
                // (fn-narrowing-variadic sprint).
                let sub_is_any_function = sub_p.is_empty() && *sv;
                let sup_is_any_function = sup_p.is_empty() && *pv;

                if sub_is_any_function && sup_is_any_function {
                    // Reflexivity: any-function <: any-function.
                    // Both have params:[] and variadic:true, so only return type matters.
                    // Special case: if both are Unknown, return true (Unknown is not reflexive
                    // in is_subtype due to early guard, but the canonical any-function type
                    // has ret:Unknown). Otherwise, check return type subtyping.
                    match (&**sub_r, &**sup_r) {
                        (Type::Unknown, Type::Unknown) => return true,
                        _ if sub_r == sup_r => return true,
                        _ => {
                            return Self::is_subtype_inner(
                                sub_r,
                                sup_r,
                                tycon_env,
                                depth + 1,
                                sigma,
                            )
                        }
                    }
                }

                if sup_is_any_function && !sub_p.is_empty() {
                    // Concrete-arity function is a subtype of "any function".
                    // No need to check return type - "any function" accepts all return types
                    // (its return type is Unknown, meaning unconstrained).
                    return true;
                }

                if sub_is_any_function {
                    // "Any function" is NOT a subtype of any other function type.
                    return false;
                }

                sv == pv
                    && sub_p.len() == sup_p.len()
                    && sub_p.iter().zip(sup_p.iter()).all(
                        |((_sp_name, sp_ty), (_pp_name, pp_ty))| {
                            Self::is_subtype_inner(pp_ty, sp_ty, tycon_env, depth + 1, sigma)
                        },
                    )
                    && Self::is_subtype_inner(sub_r, sup_r, tycon_env, depth + 1, sigma)
            }
            // Operator variables are treated like TypeVars for subtyping purposes.
            (Type::Operator(m1), Type::Operator(m2)) => m1 == m2,
            // TypeStageApp is not a subtype of anything until reduced (conservative)
            (Type::TypeStageApp { .. }, _) | (_, Type::TypeStageApp { .. }) => false,
            // NominalVariant is a subtype of another NominalVariant iff tags match and fields are compatible
            (
                Type::NominalVariant {
                    tag: tag1,
                    fields: fields1,
                },
                Type::NominalVariant {
                    tag: tag2,
                    fields: fields2,
                },
            ) => {
                // Tags must match (nominal identity)
                if tag1 != tag2 {
                    return false;
                }
                // Fields must satisfy structural subtyping (same as Record)
                for (k, sup_ty) in &fields2.fields {
                    match fields1.fields.get(k) {
                        Some(sub_ty) => {
                            if !Self::is_subtype_inner(sub_ty, sup_ty, tycon_env, depth + 1, sigma)
                            {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                true
            }
            // NominalVariant is NEVER a subtype of Record (nominal vs structural distinction)
            (Type::NominalVariant { .. }, Type::Record(_)) => false,
            (Type::Record(_), Type::NominalVariant { .. }) => false,
            _ => false,
        }
    }

    /// The AGT consistent subtyping relation (Garcia et al. 2016, Proposition 22): `A ~<: B`.
    ///
    /// Used for `value_matches_type`: ground types carry `Unknown` at erased positions
    /// (Seq elements, Map values, Dict field values, Function params/returns).
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
        if matches!(sub, Type::Error) || matches!(sup, Type::Error) {
            return false;
        }
        match (sub, sup) {
            // Primitives: exact match
            (Type::Int, Type::Int)
            | (Type::Str, Type::Str)
            | (Type::Bool, Type::Bool)
            | (Type::Float, Type::Float)
            | (Type::Bytes, Type::Bytes) => true,
            // Top accepts everything
            (_, Type::Top) => true,
            // Structural recursion — consistent subtyping throughout all composite types.
            // App covers Seq[A] ~<: Seq[B] (TyCon("Seq") head) and Map similarly.
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                Self::is_consistent_subtype(f1, f2) && Self::is_consistent_subtype(a1, a2)
            }
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (Type::Record(sub_row), Type::Record(sup_row)) => {
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
                },
                Type::Function {
                    params: sup_p,
                    ret: sup_r,
                    variadic: sup_v,
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
                sub_p.len() == sup_p.len()
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
        if matches!(t1, Type::Unknown | Type::Top | Type::Error)
            || matches!(t2, Type::Unknown | Type::Top | Type::Error)
        {
            return false;
        }

        // Different concrete primitives are disjoint
        match (t1, t2) {
            // Same type → not disjoint
            (a, b) if a == b => false,

            // Int and Float are disjoint (Number is their supertype, not intersection)
            (Type::Int, Type::Float) | (Type::Float, Type::Int) => true,
            (Type::IntLiteral(_), Type::Float) | (Type::Float, Type::IntLiteral(_)) => true,

            // Different primitives are disjoint
            (Type::Int | Type::IntLiteral(_), Type::Str | Type::StringLiteral(_)) => true,
            (Type::Int | Type::IntLiteral(_), Type::Bool) => true,
            (Type::Int | Type::IntLiteral(_), Type::Bytes) => true,
            (Type::Float, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Float, Type::Bool) => true,
            (Type::Float, Type::Bytes) => true,
            (Type::Str | Type::StringLiteral(_), Type::Bool) => true,
            (Type::Str | Type::StringLiteral(_), Type::Bytes) => true,
            (Type::Bool, Type::Bytes) => true,

            // Symmetric cases
            (Type::Str | Type::StringLiteral(_), Type::Int | Type::IntLiteral(_)) => true,
            (Type::Bool, Type::Int | Type::IntLiteral(_)) => true,
            (Type::Bytes, Type::Int | Type::IntLiteral(_)) => true,
            (Type::Str | Type::StringLiteral(_), Type::Float) => true,
            (Type::Bool, Type::Float) => true,
            (Type::Bytes, Type::Float) => true,
            (Type::Bool, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Bytes, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Bytes, Type::Bool) => true,

            // Record vs any primitive is disjoint
            (Type::Record(_), Type::Int | Type::IntLiteral(_)) => true,
            (Type::Record(_), Type::Float) => true,
            (Type::Record(_), Type::Str | Type::StringLiteral(_)) => true,
            (Type::Record(_), Type::Bool) => true,
            (Type::Record(_), Type::Bytes) => true,
            (Type::Int | Type::IntLiteral(_), Type::Record(_)) => true,
            (Type::Float, Type::Record(_)) => true,
            (Type::Str | Type::StringLiteral(_), Type::Record(_)) => true,
            (Type::Bool, Type::Record(_)) => true,
            (Type::Bytes, Type::Record(_)) => true,

            // Function vs primitives (for precise false-branch narrowing after fn? guards)
            (Type::Function { .. }, Type::Int | Type::IntLiteral(_)) => true,
            (Type::Function { .. }, Type::Float) => true,
            (Type::Function { .. }, Type::Number) => true,
            (Type::Function { .. }, Type::Str | Type::StringLiteral(_)) => true,
            (Type::Function { .. }, Type::Bool) => true,
            (Type::Function { .. }, Type::Bytes) => true,
            (Type::Int | Type::IntLiteral(_), Type::Function { .. }) => true,
            (Type::Float, Type::Function { .. }) => true,
            (Type::Number, Type::Function { .. }) => true,
            (Type::Str | Type::StringLiteral(_), Type::Function { .. }) => true,
            (Type::Bool, Type::Function { .. }) => true,
            (Type::Bytes, Type::Function { .. }) => true,

            // Function vs structural types (Record, NominalVariant, App)
            (Type::Function { .. }, Type::Record(_)) => true,
            (Type::Function { .. }, Type::App(_, _)) => true,
            (Type::Function { .. }, Type::NominalVariant { .. }) => true,
            (Type::Record(_), Type::Function { .. }) => true,
            (Type::App(_, _), Type::Function { .. }) => true,
            (Type::NominalVariant { .. }, Type::Function { .. }) => true,

            // NominalVariant vs primitives
            (Type::NominalVariant { .. }, Type::Int | Type::IntLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Float) => true,
            (Type::NominalVariant { .. }, Type::Str | Type::StringLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Bool) => true,
            (Type::NominalVariant { .. }, Type::Bytes) => true,
            (Type::NominalVariant { .. }, Type::App(_, _)) => true,
            (Type::Int | Type::IntLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Float, Type::NominalVariant { .. }) => true,
            (Type::Str | Type::StringLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Bool, Type::NominalVariant { .. }) => true,
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
            (Type::Record(row1), Type::Record(row2))
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
            (Type::NominalVariant { tag: tag1, .. }, Type::NominalVariant { tag: tag2, .. }) => {
                tag1 != tag2
            }
            // NominalVariant vs Record (both directions)
            (Type::NominalVariant { .. }, Type::Record(_)) => true,
            (Type::Record(_), Type::NominalVariant { .. }) => true,
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
        if matches!(a, Type::Error) || matches!(b, Type::Error) {
            return false;
        }
        // Structural decomposition
        match (a, b) {
            // App covers Seq[A] ~ Seq[B] (TyCon("Seq") head) and Map similarly.
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                Type::is_consistent(f1, f2) && Type::is_consistent(a1, a2)
            }
            (Type::TyCon(n1), Type::TyCon(n2)) => n1 == n2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                    variadic: v1,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                    variadic: v2,
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
            (Type::Record(row1), Type::Record(row2)) => {
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
            (Type::Record(row), Type::Intersection(members))
            | (Type::Intersection(members), Type::Record(row)) => members.iter().all(|m| {
                if let Type::Record(mrow) = m {
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
                    Type::is_consistent(&Type::Record(row.clone()), m)
                }
            }),
            // Literal types are consistent with their parent types (similar to subtyping)
            (Type::IntLiteral(_), Type::Int | Type::Number)
            | (Type::Int | Type::Number, Type::IntLiteral(_)) => true,
            (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => true,
            (Type::Int | Type::Float, Type::Number) | (Type::Number, Type::Int | Type::Float) => {
                true
            }
            // Top is consistent with everything (τ ~ Top for all τ)
            (Type::Top, _) | (_, Type::Top) => true,
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
                    tag: tag1,
                    fields: fields1,
                },
                Type::NominalVariant {
                    tag: tag2,
                    fields: fields2,
                },
            ) => {
                // Tags must match for consistency (nominal identity)
                if tag1 != tag2 {
                    return false;
                }
                // Shared fields must be consistent (same logic as Record ~ Record)
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
                if let Type::Record(row) = m {
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
                if let Type::Record(row) = m {
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
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
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
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
                fields.fields.values().any(|ty| ty.has_inference_vars())
            }
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
            Type::Record(row) => {
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
            } => {
                params.iter().any(|(_name, p_ty)| p_ty.has_type_stage_app())
                    || ret.has_type_stage_app()
            }
            Type::Union(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Intersection(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Negation(inner) => inner.has_type_stage_app(),
            Type::App(f, a) => f.has_type_stage_app() || a.has_type_stage_app(),
            Type::TyCon(_) => false,
            Type::NominalVariant { tag: _, fields } => {
                fields.fields.values().any(|ty| ty.has_type_stage_app())
            }
            // S-860: equirecursive-types-core — recurse into the body.
            Type::Recursive { var: _, body } => body.has_type_stage_app(),
            _ => false,
        }
    }

    /// Collect type variables in a single tree walk.
    /// Under BAS, row variables no longer exist (no RowVar nodes), so the  parameter
    /// has been removed.
    pub fn collect_all_vars(&self, type_vars: &mut HashSet<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.insert(name.clone());
            }
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
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
    /// This replaces the double-walk pattern of calling `type_var_occurs()` then
    /// `collect_all_vars()` separately in each U-VAR arm of `unify()`.
    /// Under BAS, row variables no longer exist, so the row_vars parameter has been removed.
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
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
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
    /// Under BAS, row variables no longer exist, so the row_vars parameter has been removed.
    pub fn collect_all_vars_vec(&self, type_vars: &mut Vec<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.push(name.clone());
            }
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
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
            | Type::Bool
            | Type::Bytes
            | Type::Number
            | Type::Proxy
            | Type::Unknown
            | Type::Top
            | Type::Error
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
            Type::Record(row) => {
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
            Type::NominalVariant { tag: _, fields } => {
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
                Type::Top => return Type::Top,
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

        // Error is absorbing: any intersection containing Error becomes Error
        if members.iter().any(|m| matches!(m, Type::Error)) {
            return Type::Error;
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
                Type::Top => {
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
            return Type::Top;
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
            Type::Union(ref members) if members.iter().any(|m| matches!(m, Type::Top)) => Type::Top,
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
                    Type::Top
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
            Type::Record(row) => {
                let fields = row
                    .fields
                    .into_iter()
                    .map(|(k, v)| (k, Type::simplify_type(v)))
                    .collect();
                Type::Record(Row {
                    fields,
                    tail: RowTail::Empty,
                })
            }
            Type::Function {
                params,
                ret,
                variadic,
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
            Type::NominalVariant { tag, fields } => {
                let simplified_fields = fields
                    .fields
                    .into_iter()
                    .map(|(k, v)| (k, Type::simplify_type(v)))
                    .collect();
                Type::NominalVariant {
                    tag,
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

    /// Construct `Seq[elem]` as `App(TyCon("Seq"), elem)`.
    pub fn seq(elem: Type) -> Self {
        Type::App(Box::new(Type::TyCon("Seq".into())), Box::new(elem))
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

    /// Destructure `Seq[elem]` → `Some(elem)` or `None`.
    pub fn as_seq(&self) -> Option<&Type> {
        if let Type::App(f, arg) = self {
            if matches!(f.as_ref(), Type::TyCon(n) if n == "Seq") {
                return Some(arg);
            }
        }
        None
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
        Type::Bool => 5,
        Type::Bytes => 6,
        Type::Number => 7,
        Type::Record(_) => 8,
        Type::Function { .. } => 9,
        Type::Proxy => 12,
        Type::TypeVar(_, _) => 13,
        Type::Unknown => 14,
        Type::Top => 15,
        Type::Error => 16,
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
        // NominalVariant: compare by tag first, then by fields (via Display for simplicity)
        (Type::NominalVariant { tag: tag1, .. }, Type::NominalVariant { tag: tag2, .. }) => {
            match tag1.cmp(tag2) {
                Ordering::Equal => a.to_string().cmp(&b.to_string()),
                other => other,
            }
        }
        // For complex types (Record, Function, App), use Display representation
        // This is not ideal but ensures stability
        (Type::Record(_), Type::Record(_))
        | (Type::Function { .. }, Type::Function { .. })
        | (Type::App(_, _), Type::App(_, _)) => a.to_string().cmp(&b.to_string()),
        _ => Ordering::Equal,
    }
}

/// Check whether a leaf (non-structural, non-compound) type is a member of a built-in
/// single-parameter type class.
///
/// This is the **single authoritative source of truth** for primitive class membership.
/// It is used in two places:
///
/// 1. `type_unify::satisfies_constraint_inner` — fast-path leaf check for the structural
///    propagation rules (Record/Union/Intersection/NominalVariant field recursion).
/// 2. `type_infer::InferState::new()` — pre-seeding `InstanceEnv` with `InstanceDecl` entries
///    so that `InstanceEnv::resolve_instance` returns the same memberships as this function
///    for user-code type-checking sessions.
///
/// When adding a new type to a class (e.g., `Equatable Bytes`), update ONLY this function.
/// The pre-seeded `InstanceDecl` entries in `InferState::new()` are built by calling this
/// function — no second update is required.
///
/// **Structural (parametric) types** (`Seq`, `Map`, `Record`, `NominalVariant`) are handled
/// by structural propagation rules in `satisfies_constraint_inner` (e.g., `Record({f: τ})`
/// satisfies `Showable` iff every field `τ` satisfies `Showable`). This function handles only
/// **leaf** (non-compound) primitive types.
///
/// **Literal types** (`IntLiteral`, `StringLiteral`) are promoted to their parent types
/// (`Int`, `Str`) before constraint checking via `promote_literal_for_constrained_var`.
/// We include them here so that constraint checking on literal-typed expressions works even
/// when promotion has not yet fired (e.g., inside structural propagation).
pub fn primitive_satisfies_constraint(ty: &Type, class_name: &str) -> bool {
    match class_name {
        // Equatable: types that support structural equality comparison ([= $a $b]).
        // Record and NominalVariant are Equatable via structural propagation (all fields
        // must be Equatable) — they are NOT listed here.
        // Bytes is a primitive Equatable — byte-sequence equality is supported at runtime.
        "Equatable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
                | Type::Bytes
        ),

        // Comparable: types that support ordering ([< $a $b], [> $a $b], etc.).
        // Comparable implies Equatable via superclass relationship.
        "Comparable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Number
        ),

        // Numeric: types that support arithmetic ([+ $a $b], [* $a $b], etc.).
        // Numeric implies Equatable via superclass relationship.
        "Numeric" => matches!(
            ty,
            Type::Int | Type::IntLiteral(_) | Type::Float | Type::Number
        ),

        // Showable: types that support string conversion ([str $a]).
        // Structural types (Seq, Map, Record) are Showable but are handled by structural
        // propagation in satisfies_constraint_inner, not listed here.
        // Bytes is a primitive Showable — str() on Bytes produces a UTF-8 string at runtime.
        "Showable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
                | Type::Bytes
        ),

        // Appendable: types that support concatenation/merge ([++ $a $b]).
        // Seq, Map, and Record are Appendable but are structural — they need InstanceDecl
        // entries with TypeVar patterns (e.g., Seq[T]) in InferState::new().
        // Str/StringLiteral are primitive leaf types, listed here.
        "Appendable" => matches!(ty, Type::Str | Type::StringLiteral(_)),

        // All other classes: not a primitive member, must go through InstanceEnv resolution
        _ => false,
    }
}

/// Check that a type is well-kinded with respect to the kind environment.
///
/// This implements the [KIND-LABEL-ERROR] kinding judgment from doc/whatif/completed/hkt-monads.md:
/// Label-kinded TypeVars (Kind::Label) cannot appear in positions expecting Kind::Type (e.g., as
/// the element type of Seq, as function parameters/return types, or as record field types).
///
/// Returns an error if any TypeVar in the type has Kind::Label in `kind_env`.
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
        Type::Record(row) => {
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
                    notes: vec![],
                })
                .into());
            }
            // Bare Kind::Arrow in a type position is also kind-incorrect.
            // Arrow kinds are for higher-order type constructors that must be fully applied.
            if matches!(kind_env.get(name.as_str()), Some(Kind::Arrow(_, _))) {
                return Err(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("kind mismatch: `{name}` is a higher-kinded type constructor but was used in a type position"),
                    span,
                    notes: vec![],
                })
                .into());
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
        Type::NominalVariant { tag: _, fields } => {
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

    // Tests for primitive_satisfies_constraint — the single authoritative source of truth
    // for which primitive types satisfy which type class constraints (T-910).

    // --- Equatable ---

    #[test]
    fn test_primitive_equatable_int() {
        assert!(primitive_satisfies_constraint(&Type::Int, "Equatable"));
    }

    #[test]
    fn test_primitive_equatable_int_literal() {
        assert!(primitive_satisfies_constraint(
            &Type::IntLiteral(42),
            "Equatable"
        ));
    }

    #[test]
    fn test_primitive_equatable_float() {
        assert!(primitive_satisfies_constraint(&Type::Float, "Equatable"));
    }

    #[test]
    fn test_primitive_equatable_str() {
        assert!(primitive_satisfies_constraint(&Type::Str, "Equatable"));
    }

    #[test]
    fn test_primitive_equatable_string_literal() {
        assert!(primitive_satisfies_constraint(
            &Type::StringLiteral("hello".into()),
            "Equatable"
        ));
    }

    #[test]
    fn test_primitive_equatable_bool() {
        assert!(primitive_satisfies_constraint(&Type::Bool, "Equatable"));
    }

    #[test]
    fn test_primitive_equatable_number() {
        assert!(primitive_satisfies_constraint(&Type::Number, "Equatable"));
    }

    #[test]
    fn test_primitive_equatable_bytes() {
        // Bytes is a primitive Equatable — byte-sequence equality is supported at runtime.
        // Regression test: T-910 silently dropped Bytes when migrating from hardcoded arms
        // to primitive_satisfies_constraint. This confirms the regression is fixed.
        assert!(primitive_satisfies_constraint(&Type::Bytes, "Equatable"));
    }

    // --- Comparable ---

    #[test]
    fn test_primitive_comparable_int() {
        assert!(primitive_satisfies_constraint(&Type::Int, "Comparable"));
    }

    #[test]
    fn test_primitive_comparable_float() {
        assert!(primitive_satisfies_constraint(&Type::Float, "Comparable"));
    }

    #[test]
    fn test_primitive_comparable_str() {
        assert!(primitive_satisfies_constraint(&Type::Str, "Comparable"));
    }

    #[test]
    fn test_primitive_comparable_bool_false() {
        // Bool is NOT Comparable — no ordering defined for booleans
        assert!(!primitive_satisfies_constraint(&Type::Bool, "Comparable"));
    }

    // --- Numeric ---

    #[test]
    fn test_primitive_numeric_int() {
        assert!(primitive_satisfies_constraint(&Type::Int, "Numeric"));
    }

    #[test]
    fn test_primitive_numeric_float() {
        assert!(primitive_satisfies_constraint(&Type::Float, "Numeric"));
    }

    #[test]
    fn test_primitive_numeric_number() {
        assert!(primitive_satisfies_constraint(&Type::Number, "Numeric"));
    }

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

    #[test]
    fn test_primitive_numeric_int_literal() {
        assert!(primitive_satisfies_constraint(
            &Type::IntLiteral(0),
            "Numeric"
        ));
    }

    #[test]
    fn test_primitive_numeric_str_false() {
        assert!(!primitive_satisfies_constraint(&Type::Str, "Numeric"));
    }

    #[test]
    fn test_primitive_numeric_bool_false() {
        assert!(!primitive_satisfies_constraint(&Type::Bool, "Numeric"));
    }

    // --- Showable ---

    #[test]
    fn test_primitive_showable_int() {
        assert!(primitive_satisfies_constraint(&Type::Int, "Showable"));
    }

    #[test]
    fn test_primitive_showable_str() {
        assert!(primitive_satisfies_constraint(&Type::Str, "Showable"));
    }

    #[test]
    fn test_primitive_showable_bool() {
        assert!(primitive_satisfies_constraint(&Type::Bool, "Showable"));
    }

    #[test]
    fn test_primitive_showable_float() {
        assert!(primitive_satisfies_constraint(&Type::Float, "Showable"));
    }

    #[test]
    fn test_primitive_showable_bytes() {
        // Bytes is a primitive Showable — str() on Bytes produces a UTF-8 string at runtime.
        // Regression test: T-910 silently dropped Bytes when migrating from hardcoded arms
        // to primitive_satisfies_constraint. This confirms the regression is fixed.
        assert!(primitive_satisfies_constraint(&Type::Bytes, "Showable"));
    }

    // --- Appendable ---

    #[test]
    fn test_primitive_appendable_str() {
        assert!(primitive_satisfies_constraint(&Type::Str, "Appendable"));
    }

    #[test]
    fn test_primitive_appendable_string_literal() {
        assert!(primitive_satisfies_constraint(
            &Type::StringLiteral("x".into()),
            "Appendable"
        ));
    }

    #[test]
    fn test_primitive_appendable_int_false() {
        assert!(!primitive_satisfies_constraint(&Type::Int, "Appendable"));
    }

    #[test]
    fn test_primitive_appendable_bytes_false() {
        // Bytes is not listed as a primitive Appendable (only Concatable via InstanceEnv)
        assert!(!primitive_satisfies_constraint(&Type::Bytes, "Appendable"));
    }

    // --- Unknown class ---

    #[test]
    fn test_primitive_unknown_class_false() {
        // Any class not explicitly listed returns false — must go through InstanceEnv
        assert!(!primitive_satisfies_constraint(&Type::Int, "Joinable"));
        assert!(!primitive_satisfies_constraint(&Type::Int, "Concatable"));
        assert!(!primitive_satisfies_constraint(
            &Type::Int,
            "NonExistentClass"
        ));
    }
}
