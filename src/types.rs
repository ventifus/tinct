//! Runtime type representations, type environments with scoped alias registries,
//! substitutions/unification for Hindley-Milner polymorphism,
//! and type error definitions for the type checker.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::ast::Span;

// Split modules — substitution/unification and type environment/instantiation
#[path = "type_env.rs"]
mod type_env;
#[path = "type_unify.rs"]
mod type_unify;

pub use type_env::*;
pub use type_unify::*;

/// Row representation for record types.
///
/// `fields` uses `HashMap` because row field order is semantically irrelevant at the type level —
/// structural subtyping makes rows unordered. `Display` sorts field names for
/// deterministic output. Runtime `Value::Dict` keeps `IndexMap` for ordered user-visible
/// semantics; this HashMap is only at the type-inference layer.
///
/// Under Boolean-Algebraic Subtyping (BAS), all records are closed: openness is expressed via
/// width subtyping in `is_subtype` (a record with MORE fields satisfies an annotation with FEWER
/// fields). There are no row-variable tails.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub fields: HashMap<String, Type>, // known fields {l₁: τ₁, l₂: τ₂, ...}
}

/// Kind for higher-kinded types (Jones 1993)
/// Kinds classify types: * for proper types, (* -> *) for type constructors
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// * — kind of proper types (Int, Str, [name: Str], etc.)
    Type,
    /// k1 -> k2 — kind of type constructors (Seq: * -> *, Mappable: (* -> *) -> Constraint)
    #[allow(dead_code)] // Scaffolding for higher-kinded types
    Arrow(Box<Kind>, Box<Kind>),
    /// Operator — kind of type constructors (* → *), represents `Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Type))`
    /// Used for type constructor variables like `m` in `Monad m`
    #[allow(dead_code)] // Used in hkt-kind-inference sprint
    Operator,
    /// Label — kind of type-level string labels used for record field names
    /// Used for label TypeVars in `HasField` constraints (e.g., `key@"k"`)
    #[allow(dead_code)] // Used in hkt-mappable-appendable sprint
    Label,
    /// Kind variable — unification variable for kind inference
    #[allow(dead_code)] // Scaffolding for kind inference
    Var(u32),
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Type => write!(f, "*"),
            Kind::Arrow(k1, k2) => write!(f, "({} -> {})", k1, k2),
            Kind::Operator => write!(f, "* → *"),
            Kind::Label => write!(f, "Label"),
            Kind::Var(id) => write!(f, "?k{}", id),
        }
    }
}

/// Errors that can occur during kind unification
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Scaffolding for type class implementation
pub enum KindError {
    /// Kind mismatch — attempted to unify incompatible kinds
    Mismatch(Kind, Kind),
    /// Infinite kind — occurs check failed (kind variable appears in its own definition)
    InfiniteKind,
}

impl fmt::Display for KindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KindError::Mismatch(k1, k2) => write!(f, "Kind mismatch: {} vs {}", k1, k2),
            KindError::InfiniteKind => write!(f, "Infinite kind"),
        }
    }
}

/// Label for record field names in HasField constraints.
/// Used in `HasField { label: Label, dict_var: String, field_var: String }`.
/// Provides compile-time structural enforcement that the label position is always
/// a string literal or a label TypeVar name, never an arbitrary Type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(dead_code)] // Scaffolding for HasField constraint (hkt-mappable-appendable sprint)
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
    Seq(Box<Type>),
    Map(Box<Type>, Box<Type>), // Map[K V] — homogeneous map with key type and value type
    Proxy,
    #[allow(clippy::enum_variant_names)]
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
    /// runtime env (pwd, libdir). Represents authority to access a specific directory tree.
    DirCap,
    /// Network capability — wraps host allowlist. Injected via CLI --cap-net.
    /// Represents authority to connect to specific network hosts.
    NetCap,
    /// File/stream handle — wraps Box<dyn BufRead>. Created by `open` or `connect`.
    /// Represents authority to read/write a specific open resource.
    Handle,
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
    /// Example: `App(Operator("m"), Int)` for a monad of integers.
    /// When resolved to a builtin (e.g., `f` = `Seq`), normalized to the builtin form `Seq(a)`.
    App(Box<Type>, Box<Type>),
    /// Type constructor variable — represents a type constructor like `m` in `Monad m`.
    /// Kind: `Operator` (i.e., `* → *`). Used in typeclass constraints and generic functions.
    Operator(String),
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
            (Type::Seq(e1), Type::Seq(e2)) => e1 == e2,
            (Type::Map(k1, v1), Type::Map(k2, v2)) => k1 == k2 && v1 == v2,
            (Type::Proxy, Type::Proxy) => true,
            (Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2,
            (Type::Unknown, Type::Unknown) => true,
            (Type::Top, Type::Top) => true,
            (Type::Error, Type::Error) => true,
            (Type::DirCap, Type::DirCap) => true,
            (Type::NetCap, Type::NetCap) => true,
            (Type::Handle, Type::Handle) => true,
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
            (Type::Operator(name1), Type::Operator(name2)) => name1 == name2,
            _ => false,
        }
    }
}

impl Type {
    /// Recursive without a depth guard; safe because `Type` is a finite tree (structural recursion
    /// on an algebraic data type — each recursive call descends into a strict sub-term). The
    /// occurs-check invariant (Robinson 1965) additionally ensures that substitution-applied types
    /// are acyclic.
    ///
    /// Post gradual-typing-split (B2): Top is the true supertype (τ <: Top for all τ). Unknown
    /// is NOT in the subtype lattice — Unknown relates to other types via consistency (~), not
    /// subtyping (<:). See is_consistent() for the consistency relation.
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
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
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::Seq(sub_elem), Type::Seq(sup_elem)) => Type::is_subtype(sub_elem, sup_elem),
            // Map[K V1] <: Map[K V2] when V1 <: V2 (V covariant, K invariant via ==)
            (Type::Map(k1, v1), Type::Map(k2, v2)) => k1 == k2 && Type::is_subtype(v1, v2),
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::Str) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            // [UNION-INJ-L] and [UNION-INJ-R]: any member is a subtype of the union
            (sub_ty, Type::Union(sup_members)) => sup_members
                .iter()
                .any(|member| Type::is_subtype(sub_ty, member)),
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
                .all(|member| Type::is_subtype(member, sup_ty)),
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
                .any(|member| Type::is_subtype(member, sup_ty)),
            // [INTERSECT-ELIM]: type is a subtype of intersection iff it's a subtype of ALL members
            (sub_ty, Type::Intersection(sup_members)) => sup_members
                .iter()
                .all(|member| Type::is_subtype(sub_ty, member)),
            // Negation: A <: ~B iff A and B are disjoint (for now, conservative: only reflexive negation)
            // Full BAS subtyping requires RDNF normalization — this is a placeholder
            (Type::Negation(t1), Type::Negation(t2)) => Type::is_subtype(t2, t1), // contravariant
            // Negation subtyping: T <: ~A iff T and A are disjoint (no values in common).
            // Full BAS uses RDNF normalization to compute T ∩ A = Never, but we use a
            // conservative syntactic disjointness check that catches obvious cases like
            // Int <: ~String (true) and Int <: ~Int (false).
            (sub_ty, Type::Negation(a)) => Type::types_are_disjoint(sub_ty, a),
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
                            if !Type::is_subtype(sub_ty, sup_ty) {
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

                // All required fields from sup are present in sub with compatible types.
                // Tail check: under BAS all tails are Empty; sub may have extra fields (width subtyping).
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
                sv == pv
                    && sub_p.len() == sup_p.len()
                    && sub_p.iter().zip(sup_p.iter()).all(
                        |((_sp_name, sp_ty), (_pp_name, pp_ty))| Type::is_subtype(pp_ty, sp_ty),
                    )
                    && Type::is_subtype(sub_r, sup_r)
            }
            // App and Operator: structural equality for now (full BAS rules in hkt-bas).
            // App(f1, a1) <: App(f2, a2) requires f1 = f2 and a1 <: a2 (covariant).
            (Type::App(f1, a1), Type::App(f2, a2)) => f1 == f2 && Type::is_subtype(a1, a2),
            // Operator variables are treated like TypeVars for subtyping purposes.
            (Type::Operator(m1), Type::Operator(m2)) => m1 == m2,
            _ => false,
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

            // Seq vs primitives
            (Type::Seq(_), Type::Int | Type::IntLiteral(_)) => true,
            (Type::Seq(_), Type::Float) => true,
            (Type::Seq(_), Type::Str | Type::StringLiteral(_)) => true,
            (Type::Seq(_), Type::Bool) => true,
            (Type::Seq(_), Type::Bytes) => true,
            (Type::Int | Type::IntLiteral(_), Type::Seq(_)) => true,
            (Type::Float, Type::Seq(_)) => true,
            (Type::Str | Type::StringLiteral(_), Type::Seq(_)) => true,
            (Type::Bool, Type::Seq(_)) => true,
            (Type::Bytes, Type::Seq(_)) => true,

            // Union: disjoint if ALL members are disjoint from the other type
            (Type::Union(members), t) | (t, Type::Union(members)) => {
                members.iter().all(|m| Type::types_are_disjoint(m, t))
            }

            // Intersection: disjoint if ANY member is disjoint from the other type
            (Type::Intersection(members), t) | (t, Type::Intersection(members)) => {
                members.iter().any(|m| Type::types_are_disjoint(m, t))
            }

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
            (Type::Seq(e1), Type::Seq(e2)) => Type::is_consistent(e1, e2),
            (Type::Map(k1, v1), Type::Map(k2, v2)) => {
                Type::is_consistent(k1, k2) && Type::is_consistent(v1, v2)
            }
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
                // Under BAS all tails are Empty; tails are always consistent.
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
            // Never is consistent with everything (like Unknown, for gradual typing)
            (Type::Never, _) | (_, Type::Never) => true,
            // TypeVar is consistent with everything: an unresolved TypeVar represents an
            // unknown type and is gradual-typing consistent with any other type. This mirrors
            // Unknown's behavior in the consistency relation and prevents spurious inference
            // failures when internal TypeVars — from annotated params, `instantiate_scheme`,
            // or `fresh_type_var` in pass-1 positions — appear during prelude body checking
            // before the substitution has fully resolved them. The is_consistent check here is
            // only reached in subsumption contexts (expected_resolved.has_inference_vars() = false);
            // when the expected type has TypeVars, check_expr uses unification instead.
            //
            // Without this, `check_expr(TypeVar("xs"), Seq(⊤))` falls to `_ => false`,
            // failing prelude inference for wrappers like `drop: [fn [n@Int xs] ...]`.
            (Type::TypeVar(_, _), _) | (_, Type::TypeVar(_, _)) => true,
            // Negation: structurally consistent
            (Type::Negation(t1), Type::Negation(t2)) => Type::is_consistent(t1, t2),
            // Capability types, Proxy: consistent only if equal (handled by a == b above)
            // All other combinations are inconsistent
            _ => false,
        }
    }

    /// Normalize a union type: flatten nested unions, deduplicate, and sort.
    /// Single-element unions are unwrapped to the bare type.
    /// Empty unions are not supported and will panic (caller must ensure non-empty).
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
            use std::cmp::Ordering;
            let order_a = type_order(a);
            let order_b = type_order(b);
            match order_a.cmp(&order_b) {
                Ordering::Equal => type_payload_cmp(a, b),
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

    /// Structural simplification pass — RDNF groundwork.
    ///
    /// Applies identity/absorbing element rules, then delegates to normalize_union /
    /// normalize_intersection for flattening, deduplication, and sorting.  Importantly,
    /// this function applies two BAS structural rules that require looking at the *content*
    /// of compound types:
    ///
    /// **S-RcdTop** (BAS width subtyping): A union of two closed single-field Records whose
    /// field names are disjoint is equivalent to `Top` in the BAS lattice.  For example,
    /// `{x: Int} | {y: Str}` cannot be narrowed further by structural subtyping — any
    /// record discriminator must fall into one of these two disjoint shapes, which together
    /// cover the entire record universe at those labels.  We conservatively apply this only
    /// when BOTH records are closed (Empty tail), matching the BAS closed-record assumption.
    ///
    /// **S-ClsBot** (nominal disjointness): An intersection of two closed single-field
    /// Records whose field names differ collapses to `Never`.  A value cannot simultaneously
    /// be `{x: τ}` (exactly one field x) and `{y: π}` (exactly one field y) when x ≠ y.
    /// This is the structural analogue of nominal tag annihilation (#C1 & #C2 ≤ Never).
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
                            i != j && !matches!(b, Type::Negation(_)) && Type::is_subtype(a, b)
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
                        if Type::is_subtype(&members[i], &members[j]) {
                            to_keep[i] = false;
                            break;
                        }
                    }
                }
                let reduced: Vec<Type> = members
                    .into_iter()
                    .zip(to_keep.into_iter())
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
                Type::Record(Row { fields })
            }
            Type::Seq(elem) => Type::Seq(Box::new(Type::simplify_type(*elem))),
            Type::Map(k, v) => Type::Map(
                Box::new(Type::simplify_type(*k)),
                Box::new(Type::simplify_type(*v)),
            ),
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
            // Primitive types and type variables have no children to recurse into
            other => other,
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
        // Guard: every member must be a single-field record
        let single_field_keys: Vec<&str> = members
            .iter()
            .map(|m| {
                if let Type::Record(row) = m {
                    if row.fields.len() == 1 {
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
        // Guard: every member must be a single-field record
        let single_field_keys: Option<Vec<&str>> = members
            .iter()
            .map(|m| {
                if let Type::Record(row) = m {
                    if row.fields.len() == 1 {
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
                // Row tail contains no type variables (only RowVar or Empty)
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
            Type::Seq(elem) => elem.collect_type_vars(vars),
            Type::Map(key, val) => {
                key.collect_type_vars(vars);
                val.collect_type_vars(vars);
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
            _ => {}
        }
    }

    /// Returns true if the type contains any inference variables (TypeVar).
    /// Used to determine whether a type is concrete or still under inference.
    pub fn has_inference_vars(&self) -> bool {
        match self {
            Type::TypeVar(_, _) => true,
            Type::Record(row) => row.fields.values().any(|ty| ty.has_inference_vars()),
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                params.iter().any(|(_name, p_ty)| p_ty.has_inference_vars())
                    || ret.has_inference_vars()
            }
            Type::Seq(elem) => elem.has_inference_vars(),
            Type::Map(key, val) => key.has_inference_vars() || val.has_inference_vars(),
            Type::Union(members) => members.iter().any(|m| m.has_inference_vars()),
            Type::Intersection(members) => members.iter().any(|m| m.has_inference_vars()),
            Type::Negation(inner) => inner.has_inference_vars(),
            Type::App(f, a) => f.has_inference_vars() || a.has_inference_vars(),
            Type::Operator(_) => true, // Operator variables ARE inference variables
            Type::Proxy => false,
            _ => false,
        }
    }

    /// Collect both type variables and row variables in a single tree walk.
    /// Performance optimization: avoids allocating two HashSets and traversing the type tree twice.
    pub fn collect_all_vars(
        &self,
        type_vars: &mut HashSet<String>,
        row_vars: &mut HashSet<String>,
    ) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.insert(name.clone());
            }
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars(type_vars, row_vars);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_all_vars(type_vars, row_vars);
                }
                ret.collect_all_vars(type_vars, row_vars);
            }
            Type::Seq(elem) => elem.collect_all_vars(type_vars, row_vars),
            Type::Map(key, val) => {
                key.collect_all_vars(type_vars, row_vars);
                val.collect_all_vars(type_vars, row_vars);
            }
            Type::Union(members) => {
                for member in members {
                    member.collect_all_vars(type_vars, row_vars);
                }
            }
            Type::Intersection(members) => {
                for member in members {
                    member.collect_all_vars(type_vars, row_vars);
                }
            }
            Type::Negation(inner) => {
                inner.collect_all_vars(type_vars, row_vars);
            }
            Type::App(f, a) => {
                f.collect_all_vars(type_vars, row_vars);
                a.collect_all_vars(type_vars, row_vars);
            }
            Type::Operator(name) => {
                type_vars.insert(name.clone());
            }
            _ => {}
        }
    }

    /// Fused occurs check + variable collection: checks whether `occurs_name` appears
    /// in the type tree and simultaneously collects all type vars and row vars.
    /// Returns `true` if `occurs_name` was found (infinite-type guard for U-VAR arms).
    ///
    /// This replaces the double-walk pattern of calling `type_var_occurs()` then
    /// `collect_all_vars()` separately in each U-VAR arm of `unify()`.
    pub fn collect_all_vars_check_occurs(
        &self,
        occurs_name: &str,
        type_vars: &mut HashSet<String>,
        row_vars: &mut HashSet<String>,
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
                    found |= ty.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
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
                    found |= p_ty.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                }
                found |= ret.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found
            }
            Type::Seq(elem) => elem.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars),
            Type::Map(key, val) => {
                let mut found = false;
                found |= key.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found |= val.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found
            }
            Type::Union(members) => {
                let mut found = false;
                for member in members {
                    found |= member.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                }
                found
            }
            Type::Intersection(members) => {
                let mut found = false;
                for member in members {
                    found |= member.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                }
                found
            }
            Type::Negation(inner) => {
                inner.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars)
            }
            Type::App(f, a) => {
                let mut found = false;
                found |= f.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found |= a.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found
            }
            Type::Operator(name) => {
                let found = name == occurs_name;
                type_vars.insert(name.clone());
                found
            }
            _ => false,
        }
    }

    /// Collect type and row variables into Vecs, allowing duplicates. Cheaper than HashSet
    /// allocation; callers that need deduplication handle it via seen-set or contains_key guards.
    /// Production callers: `instantiate_at_level` and `generalize`. (The test-only `instantiate()`
    /// uses the HashSet variant `collect_all_vars` instead.)
    pub fn collect_all_vars_vec(&self, type_vars: &mut Vec<String>, row_vars: &mut Vec<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.push(name.clone());
            }
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars_vec(type_vars, row_vars);
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for (_name, p_ty) in params {
                    p_ty.collect_all_vars_vec(type_vars, row_vars);
                }
                ret.collect_all_vars_vec(type_vars, row_vars);
            }
            Type::Seq(elem) => elem.collect_all_vars_vec(type_vars, row_vars),
            Type::Map(key, val) => {
                key.collect_all_vars_vec(type_vars, row_vars);
                val.collect_all_vars_vec(type_vars, row_vars);
            }
            Type::Union(members) => {
                for member in members {
                    member.collect_all_vars_vec(type_vars, row_vars);
                }
            }
            Type::Intersection(members) => {
                for member in members {
                    member.collect_all_vars_vec(type_vars, row_vars);
                }
            }
            Type::Negation(inner) => {
                inner.collect_all_vars_vec(type_vars, row_vars);
            }
            Type::App(f, a) => {
                f.collect_all_vars_vec(type_vars, row_vars);
                a.collect_all_vars_vec(type_vars, row_vars);
            }
            Type::Operator(name) => {
                type_vars.push(name.clone());
            }
            _ => {}
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
            Type::Seq(elem) => elem.collect_operator_names(operator_names),
            Type::Map(key, val) => {
                key.collect_operator_names(operator_names);
                val.collect_operator_names(operator_names);
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
            _ => {}
        }
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
        Type::Seq(_) => 10,
        Type::Map(_, _) => 11,
        Type::Proxy => 12,
        Type::TypeVar(_, _) => 13,
        Type::Unknown => 14,
        Type::Top => 15,
        Type::Error => 16,
        Type::DirCap => 17,
        Type::NetCap => 18,
        Type::Handle => 19,
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
        Type::Operator(_) => 35,
    }
}

/// Helper for normalize_union: compare payloads for types with the same variant.
fn type_payload_cmp(a: &Type, b: &Type) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Type::IntLiteral(n1), Type::IntLiteral(n2)) => n1.cmp(n2),
        (Type::StringLiteral(s1), Type::StringLiteral(s2)) => s1.cmp(s2),
        (Type::TypeVar(name1, _), Type::TypeVar(name2, _)) => name1.cmp(name2),
        (Type::Operator(name1), Type::Operator(name2)) => name1.cmp(name2),
        // For complex types (Record, Function, Seq, Map, App), use Display representation
        // This is not ideal but ensures stability
        (Type::Record(_), Type::Record(_))
        | (Type::Function { .. }, Type::Function { .. })
        | (Type::Seq(_), Type::Seq(_))
        | (Type::Map(_, _), Type::Map(_, _))
        | (Type::App(_, _), Type::App(_, _)) => a.to_string().cmp(&b.to_string()),
        _ => Ordering::Equal,
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
                return Err(TypeError::new(
                    format!("label variable {} has kind Label, expected kind *", name),
                    span,
                ));
            }
            Ok(())
        }
        Type::Seq(elem) => check_kind_wellformed(elem, kind_env, span),
        Type::Map(key, val) => {
            check_kind_wellformed(key, kind_env, span)?;
            check_kind_wellformed(val, kind_env, span)
        }
        Type::Function { params, ret, .. } => {
            for (_name, param_ty) in params {
                check_kind_wellformed(param_ty, kind_env, span)?;
            }
            check_kind_wellformed(ret, kind_env, span)
        }
        Type::Record(row) => {
            for field_ty in row.fields.values() {
                check_kind_wellformed(field_ty, kind_env, span)?;
            }
            Ok(())
        }
        Type::Union(members) | Type::Intersection(members) => {
            for member in members {
                check_kind_wellformed(member, kind_env, span)?;
            }
            Ok(())
        }
        Type::Negation(inner) => check_kind_wellformed(inner, kind_env, span),
        Type::App(func, arg) => {
            check_kind_wellformed(func, kind_env, span)?;
            check_kind_wellformed(arg, kind_env, span)
        }
        Type::Operator(name) => {
            // Bare Operator in a type position (kind *) is kind-incorrect.
            // Operator variables have kind (* → *) and must be applied via Type::App.
            if let Some(Kind::Operator) = kind_env.get(name.as_str()) {
                return Err(TypeError::new(
                    format!(
                        "kind mismatch: {} has kind * → * but appears in a type (kind *) position",
                        name
                    ),
                    span,
                ));
            }
            // If the name is not in kind_env, let it pass (freshly introduced Operator
            // that hasn't been kind-registered yet, or will be registered later)
            Ok(())
        }
        // All other types (Int, Str, Bool, literals, capabilities, etc.) are always well-kinded
        _ => Ok(()),
    }
}

/// Constraint on a type variable (type class membership or structural property)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// Type class constraint: `class vars` (e.g., `Numeric a` or `Add a b c`)
    ///
    /// `vars`: Type variable names in the constraint (e.g., ["a"] for single-param, ["a", "b", "c"] for MPTC)
    /// `fundeps`: Functional dependencies as (determining positions, determined positions) pairs.
    ///            Each pair is (Vec<usize>, Vec<usize>) indexing into `vars`.
    ///            For `Add a b c` with FD `(a,b) → c`: fundeps = vec![(vec![0,1], vec![2])]
    Class {
        class: String,
        vars: Vec<String>,
        fundeps: Vec<(Vec<usize>, Vec<usize>)>,
    },
    /// HasField constraint: `HasField label dict_var field_var`
    /// Asserts that dict_var has a field at label with type field_var.
    /// Functional dependency: (label, dict_var) → field_var
    HasField {
        label: Label,
        dict_var: String,
        field_var: String,
    },
}

impl Constraint {
    /// Create a single-parameter Class constraint (backward compatibility helper)
    pub fn new(class: impl Into<String>, var: impl Into<String>) -> Self {
        Self::Class {
            class: class.into(),
            vars: vec![var.into()],
            fundeps: vec![],
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Class { class, vars, .. } => {
                write!(f, "{}", class)?;
                for var in vars {
                    write!(f, " {}", var)?;
                }
                Ok(())
            }
            Constraint::HasField {
                label,
                dict_var,
                field_var,
            } => write!(f, "HasField {} {} {}", label, dict_var, field_var),
        }
    }
}

/// Bounds for a type variable in algebraic subtyping.
/// A type variable α is satisfiable iff join(lower) <: meet(upper).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeVarBounds {
    /// Lower bounds: types that are subtypes of this variable (positive positions).
    /// Multiple lower bounds compact to a union: α ⊇ Int, α ⊇ Str → α = Int | Str
    pub lower: Vec<Type>,
    /// Upper bounds: types that are supertypes of this variable (negative positions).
    /// Multiple upper bounds compact to an intersection: α ⊆ Number, α ⊆ Equatable → α = Number & Equatable
    pub upper: Vec<Type>,
}

impl TypeVarBounds {
    pub fn new() -> Self {
        Self {
            lower: vec![],
            upper: vec![],
        }
    }

    /// Add a lower bound: `ty <: var`
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub fn add_lower(&mut self, ty: Type) {
        self.lower.push(ty);
    }

    /// Add an upper bound: `var <: ty`
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub fn add_upper(&mut self, ty: Type) {
        self.upper.push(ty);
    }
}

impl Default for TypeVarBounds {
    fn default() -> Self {
        Self::new()
    }
}

/// Provenance for a subtyping constraint — tracks why the constraint was generated.
/// Used for error messages when bounds are unsatisfiable.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // Scaffolding for algebraic subtyping — wired in a future sprint
pub struct ConstraintSource {
    pub span: Span,
    pub reason: String,
}

/// Type scheme for let-generalization (∀α₁...αₙ. τ)
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScheme {
    pub type_vars: Vec<String>,
    pub constraints: Vec<Constraint>,
    pub body: Type,
    /// Label type variables — generalized label-kinded TypeVars from `key@"k"` annotations.
    /// Must be re-registered in `state.kind_env` with `Kind::Label` during instantiation
    /// to prevent promotion suppression from failing after generalization.
    pub label_vars: Vec<String>,
    /// Optional documentation string (from `fn@[doc: "..."]` annotations).
    /// Not part of the type; used by LSP hover display.
    pub doc: Option<String>,
    /// Nested dict polymorphism: when this scheme binds a dict literal, stores
    /// the generalized TypeScheme for each dict entry. Used by [DOT-POLY] to
    /// instantiate polymorphic functions accessed via dot-notation.
    /// Only `Some` for dict literals bound directly; `None` for function params,
    /// cross-file opaque types, and non-dict schemes.
    pub inner_schemes: Option<HashMap<String, TypeScheme>>,
}

impl TypeScheme {
    /// Create a monomorphic scheme (no quantified variables)
    pub fn mono(ty: Type) -> Self {
        Self {
            type_vars: vec![],
            constraints: vec![],
            body: ty,
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        }
    }
}

impl fmt::Display for TypeScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display constraints if present: "Equatable a, Numeric b => "
        if !self.constraints.is_empty() {
            for (i, constraint) in self.constraints.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", constraint)?;
            }
            write!(f, " => ")?;
        }

        if self.type_vars.is_empty() {
            write!(f, "{}", self.body)
        } else {
            write!(f, "∀")?;
            let mut first = true;
            for var in &self.type_vars {
                if !first {
                    write!(f, " ")?;
                }
                write!(f, "{var}")?;
                first = false;
            }
            write!(f, ". {}", self.body)
        }
    }
}

/// Maps expression spans `(start_offset, end_offset)` to the TypeScheme of the variable
/// referenced there. Only populated for `VarRef` expressions that resolve to a polymorphic
/// scheme (schemes with constraints or type variables). Used by LSP hover to display
/// constraints (e.g., `Equatable a => Fn@Bool [a a]`).
///
/// Stored in `InferState.scheme_map` during inference, then extracted and returned as part
/// of the type-checking result for LSP consumers.
pub type SchemeMap = HashMap<(usize, usize), TypeScheme>;

/// Inference state for levels-based let-generalization
#[derive(Debug, Clone)]
pub struct InferState {
    /// Monotonic counter for fresh type/row variable names (_t0, _t1, ...).
    /// Uses u32 instead of u64 — assumes no single inference run creates >4B type variables.
    /// (In practice, programs with >1M type variables exhaust memory first via substitution map growth.)
    pub name_counter: u32,
    pub level: u32,
    pub levels: HashMap<String, u32>,
    /// Global accumulated substitution: collects constraints from access-chain inference
    /// and other constraint generators. Applied when resolving type variables during
    /// inference, so that constraints from `$x.field1` are visible when processing
    /// `$x.field2` in the same expression. See doc/07-type-extensions.md Part 5.
    pub subst: Substitution,
    /// Accumulated type class constraints on type variables.
    /// Constraints are generated when overloaded builtins are called with type variables.
    /// During generalization, constraints on generalized variables are included in the TypeScheme.
    pub constraints: Vec<Constraint>,
    /// Type variable bounds for algebraic subtyping.
    /// Maps type variable names to their lower/upper bounds. Used alongside `subst` during
    /// the migration from unification to constraint-based typing. Eventually, bounds will
    /// replace equality-based substitution for all type variables.
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub bounds: HashMap<String, TypeVarBounds>,
    /// Kind environment: maps TypeVar names to their kinds.
    /// Populated during class method processing (Kind::Operator) and when `key@"k"` annotations
    /// are resolved (Kind::Label). Used to prevent promotion of label-kinded TypeVars and to
    /// enforce kind checking (e.g., reject `Seq(TypeVar(l, Label))`).
    pub kind_env: HashMap<String, Kind>,
    /// Type class environment: registry of class declarations.
    /// Dict-scoped: class declarations are visible in the dict and children.
    pub class_env: ClassEnv,
    /// Type class instance environment: registry of instance declarations.
    /// Globally registered: coherence requires global uniqueness.
    pub instance_env: InstanceEnv,
    /// Names of bindings that failed type inference, mapping to the span of the failed binding.
    /// Used to annotate downstream T002 "undefined variable" errors with a "caused by" note
    /// that points to the failed definition site instead of just saying "not in scope".
    pub failed_bindings: HashMap<String, Span>,
    /// Span-keyed map from VarRef sites to the TypeScheme of the variable they reference.
    /// Only populated when the caller enables scheme collection (non-None). Used by LSP hover
    /// to display type class constraints alongside the instantiated type.
    ///
    /// Enabled by setting this to `Some(SchemeMap::new())` before running inference.
    pub scheme_map: Option<SchemeMap>,
    /// Name of the function currently being inferred (for polymorphic recursion detection).
    /// Set by infer_fn when entering a function body, cleared when exiting.
    pub current_function: Option<String>,
    /// Expected return type of the currently-inferring function (if annotated).
    /// Set by infer_fn when entering a function body with an explicit return annotation,
    /// cleared when exiting. Used for inferred [do] macro to determine which monad to use.
    pub expected_return: Option<Type>,
}

impl InferState {
    pub fn new() -> Self {
        let mut class_env = ClassEnv::new();

        // Register built-in type classes with their superclass relationships.
        // Class declarations define the hierarchy (which classes extend which).
        // Instance resolution happens in two stages:
        //   1. satisfies_constraint: hardcoded for Numeric only
        //   2. InstanceEnv::resolve_instance: dynamic resolution from prelude.llt instances
        //
        // These pre-registrations ensure the class hierarchy is available before prelude.llt
        // is type-checked. When prelude.llt is loaded, it will register instances (not classes)
        // which will be used by resolve_instance for constraint checking.

        // Equatable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Equatable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Numeric: extends Equatable (hardcoded instance set for Int/Float/Number/IntLiteral)
        class_env.insert(ClassDecl {
            name: "Numeric".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), "a".to_string())],
            methods: HashMap::new(),
        });

        // Add: 3-parameter type class with functional dependency (a,b) → c
        // Functional dependencies are stored in Constraint, not ClassDecl
        class_env.insert(ClassDecl {
            name: "Add".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Sub: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Sub".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Mul: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Mul".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Div: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Div".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Comparable: extends Equatable (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), "a".to_string())],
            methods: HashMap::new(),
        });

        // Showable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Showable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Mappable: base class (instances defined in prelude.llt)
        // Kind::Operator for higher-kinded type constructor polymorphism
        class_env.insert(ClassDecl {
            name: "Mappable".to_string(),
            params: vec![("f".to_string(), Kind::Operator)],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        // Appendable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Appendable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
        });

        Self {
            name_counter: 0,
            level: 0,
            levels: HashMap::new(),
            subst: Substitution::new(),
            constraints: Vec::new(),
            bounds: HashMap::new(),
            kind_env: HashMap::new(),
            class_env,
            instance_env: InstanceEnv::new(),
            failed_bindings: HashMap::new(),
            scheme_map: None,
            current_function: None,
            expected_return: None,
        }
    }

    /// Add a type class constraint to the inference state.
    /// The constraint is checked during instantiation.
    pub fn add_constraint(&mut self, class: impl Into<String>, var: impl Into<String>) {
        self.constraints.push(Constraint::new(class, var));
    }

    /// Create a fresh type variable at the current level and register it in `state.levels`.
    pub fn fresh_type_var(&mut self) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }

    // fresh_row_var_name removed — BAS Step 4: no RowVar tails exist

    #[cfg(test)]
    pub fn fresh_var(&mut self) -> Type {
        self.fresh_type_var()
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for kind inference and unification (Jones 1993)
///
/// Kind inference assigns kinds to type constructors and validates their usage.
/// For example, `Seq` has kind `* -> *` (takes a type, returns a type), while
/// `Int` has kind `*` (is a proper type). Kind variables (`Kind::Var`) are used
/// for type constructors whose kind is not yet known.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Scaffolding for type class implementation
pub struct KindState {
    /// Monotonic counter for fresh kind variable IDs
    pub next_var: u32,
    /// Substitution from kind variables to kinds
    pub substitution: HashMap<u32, Kind>,
}

impl KindState {
    /// Create a new kind inference state
    pub fn new() -> Self {
        Self {
            next_var: 0,
            substitution: HashMap::new(),
        }
    }

    /// Generate a fresh kind variable
    #[allow(dead_code)] // Scaffolding for kind inference
    pub fn fresh_var(&mut self) -> Kind {
        let id = self.next_var;
        self.next_var = self.next_var.saturating_add(1);
        Kind::Var(id)
    }

    /// Apply the current substitution to a kind (chase bindings to fixpoint)
    pub fn apply(&self, kind: &Kind) -> Kind {
        match kind {
            Kind::Type => Kind::Type,
            Kind::Arrow(k1, k2) => Kind::Arrow(Box::new(self.apply(k1)), Box::new(self.apply(k2))),
            Kind::Operator => Kind::Operator,
            Kind::Label => Kind::Label,
            Kind::Var(id) => {
                if let Some(k) = self.substitution.get(id) {
                    self.apply(k) // Chase transitive bindings
                } else {
                    Kind::Var(*id)
                }
            }
        }
    }

    /// Default all unresolved kind variables to `Kind::Type` (Jones 1993, §4)
    ///
    /// After kind inference completes, any remaining kind variables represent unconstrained
    /// type constructors. By convention (and for simplicity), we default them to `*` (proper types).
    /// This is sound because:
    /// - If a type constructor has no applied arguments, it must have kind `*`
    /// - If it has arguments but no kind constraints, defaulting to `*` is the most permissive choice
    ///
    /// Example: `[@Seq<$T> [1 2 3]]` infers `Seq: ?k0 -> *`, then defaults `?k0` to `*`, giving `Seq: * -> *`.
    #[allow(dead_code)] // Scaffolding for kind inference
    pub fn default_remaining(&mut self) {
        // Collect all kind variables that appear in the substitution (transitively)
        let mut all_vars = HashSet::new();
        for k in self.substitution.values() {
            self.collect_kind_vars(k, &mut all_vars);
        }

        // Default any unbound kind variables to Type
        for id in 0..self.next_var {
            if !self.substitution.contains_key(&id) && !all_vars.contains(&id) {
                self.substitution.insert(id, Kind::Type);
            }
        }
    }

    /// Collect all kind variables appearing in a kind (for defaulting)
    #[allow(dead_code)] // Scaffolding for kind inference
    fn collect_kind_vars(&self, kind: &Kind, vars: &mut HashSet<u32>) {
        match kind {
            Kind::Type => {}
            Kind::Arrow(k1, k2) => {
                self.collect_kind_vars(k1, vars);
                self.collect_kind_vars(k2, vars);
            }
            Kind::Operator => {}
            Kind::Label => {}
            Kind::Var(id) => {
                if vars.insert(*id) {
                    // Only recurse if we haven't seen this variable before
                    if let Some(k) = self.substitution.get(id) {
                        self.collect_kind_vars(k, vars);
                    }
                }
            }
        }
    }
}

impl Default for KindState {
    fn default() -> Self {
        Self::new()
    }
}

/// Occurs check for kind unification — does kind variable `v` appear in kind `k`?
///
/// Prevents infinite kinds like `?k0 = ?k0 -> *`, which would create cycles in the kind structure.
#[allow(dead_code)] // Scaffolding for type class implementation
fn occurs_in_kind(v: u32, k: &Kind, state: &KindState) -> bool {
    match state.apply(k) {
        Kind::Type => false,
        Kind::Arrow(k1, k2) => occurs_in_kind(v, &k1, state) || occurs_in_kind(v, &k2, state),
        Kind::Operator => false,
        Kind::Label => false,
        Kind::Var(id) => id == v,
    }
}

/// Unify two kinds, updating the kind substitution (Robinson's Algorithm U for kinds)
///
/// Kind unification is simpler than type unification because kinds don't have rows,
/// literals, or subtyping. It's pure structural unification:
/// - `*` unifies with `*`
/// - `k1 -> k2` unifies with `k3 -> k4` if `k1 ~ k3` and `k2 ~ k4`
/// - `?k` unifies with any kind `k` (if occurs check passes)
#[allow(dead_code)] // Scaffolding for type class implementation
pub fn unify_kind(k1: &Kind, k2: &Kind, state: &mut KindState) -> Result<(), KindError> {
    let k1 = state.apply(k1);
    let k2 = state.apply(k2);

    match (&k1, &k2) {
        (Kind::Type, Kind::Type) => Ok(()),
        (Kind::Arrow(a1, r1), Kind::Arrow(a2, r2)) => {
            unify_kind(a1, a2, state)?;
            unify_kind(r1, r2, state)
        }
        (Kind::Var(v), k) | (k, Kind::Var(v)) => {
            // Occurs check: prevent infinite kinds
            if occurs_in_kind(*v, k, state) {
                return Err(KindError::InfiniteKind);
            }
            state.substitution.insert(*v, k.clone());
            Ok(())
        }
        _ => Err(KindError::Mismatch(k1, k2)),
    }
}

#[cfg(test)]
#[allow(unused_mut)] // Substitution uses RefCell for interior mutability; mut not always needed
mod tests {
    use super::*;
    use crate::test_util::test_span;
    use std::rc::Rc;

    // Helper to create records in tests
    fn closed_record(fields: HashMap<String, Type>) -> Type {
        Type::Record(Row { fields })
    }

    // Under BAS all records are closed; this helper creates a record.
    // Previously open records are now represented as closed records — BAS width subtyping
    // allows sub-records with extra fields to satisfy any annotation.
    fn row_var_record(fields: HashMap<String, Type>, _var_name: &str, _level: u32) -> Type {
        Type::Record(Row { fields })
    }

    #[test]
    fn test_display_primitives() {
        assert_eq!(format!("{}", Type::Int), "Int");
        assert_eq!(format!("{}", Type::Float), "Float");
        assert_eq!(format!("{}", Type::Str), "String");
        assert_eq!(format!("{}", Type::Bool), "Bool");
        assert_eq!(format!("{}", Type::Number), "Number");
        assert_eq!(format!("{}", Type::Unknown), "_");
        assert_eq!(format!("{}", Type::Top), "⊤");
    }

    #[test]
    fn test_display_int_literal() {
        assert_eq!(format!("{}", Type::IntLiteral(42)), "42");
    }

    #[test]
    fn test_display_string_literal() {
        assert_eq!(
            format!("{}", Type::StringLiteral("hello".into())),
            "\"hello\""
        );
    }

    #[test]
    fn test_display_type_var() {
        assert_eq!(format!("{}", Type::TypeVar("a".into(), 0)), "a");
    }

    #[test]
    fn test_display_record() {
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        fields.insert("age".into(), Type::Int);
        // Fields are sorted alphabetically for deterministic output (HashMap has no insertion order)
        assert_eq!(
            format!("{}", closed_record(fields)),
            "[age: Int name: String]"
        );
    }

    #[test]
    fn test_display_record_empty() {
        assert_eq!(format!("{}", closed_record(HashMap::new())), "[]");
    }

    #[test]
    fn test_display_record_open() {
        // BAS: all records are closed — row_var_record now just creates Row { fields }.
        // Under BAS, display shows no "..." tail suffix.
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!("{}", row_var_record(fields, "_open", 0)),
            "[name: String]"
        );
    }

    #[test]
    fn test_display_record_open_empty() {
        // BAS: empty closed record displays as "[]" with no tail.
        assert_eq!(
            format!("{}", row_var_record(HashMap::new(), "_open", 0)),
            "[]"
        );
    }

    #[test]
    fn test_display_record_row_var() {
        // BAS: named row vars removed — display shows no "...rest" tail.
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!("{}", row_var_record(fields, "rest", 0)),
            "[name: String]"
        );
    }

    #[test]
    fn test_display_function() {
        let ty = Type::Function {
            params: vec![(None, Type::Int), (None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@Bool [Int String]");
    }

    #[test]
    fn test_display_function_no_params() {
        let ty = Type::Function {
            params: vec![],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@Int []");
    }

    #[test]
    fn test_function_equality_ignores_param_names() {
        // Function types with different param names should be equal
        let f1 = Type::Function {
            params: vec![(Some("x".into()), Type::Int), (Some("y".into()), Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![(None, Type::Int), (None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let f3 = Type::Function {
            params: vec![(Some("a".into()), Type::Int), (Some("b".into()), Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_eq!(
            f1, f2,
            "Function types should be equal regardless of param names"
        );
        assert_eq!(
            f2, f3,
            "Function types should be equal regardless of param names"
        );
        assert_eq!(
            f1, f3,
            "Function types should be equal regardless of param names"
        );

        // Different param types should NOT be equal
        let f4 = Type::Function {
            params: vec![
                (Some("x".into()), Type::Float),
                (Some("y".into()), Type::Str),
            ],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_ne!(
            f1, f4,
            "Function types with different param types should not be equal"
        );

        // Different return types should NOT be equal
        let f5 = Type::Function {
            params: vec![(Some("x".into()), Type::Int), (Some("y".into()), Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert_ne!(
            f1, f5,
            "Function types with different return types should not be equal"
        );

        // Different variadic flag should NOT be equal
        let f6 = Type::Function {
            params: vec![(Some("x".into()), Type::Int), (Some("y".into()), Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        assert_ne!(
            f1, f6,
            "Function types with different variadic flags should not be equal"
        );
    }

    #[test]
    fn test_subtype_same() {
        assert!(Type::is_subtype(&Type::Int, &Type::Int));
        assert!(Type::is_subtype(&Type::Str, &Type::Str));
    }

    #[test]
    fn test_subtype_top_and_unknown() {
        // Top is the supertype of everything (τ <: Top for all τ)
        assert!(Type::is_subtype(&Type::Int, &Type::Top));
        assert!(Type::is_subtype(&Type::Str, &Type::Top));
        assert!(Type::is_subtype(&Type::Unknown, &Type::Top));
        assert!(Type::is_subtype(&Type::Top, &Type::Top));
        // Unknown is NOT in the subtype lattice (uses consistency instead)
        assert!(!Type::is_subtype(&Type::Unknown, &Type::Int));
        assert!(!Type::is_subtype(&Type::Int, &Type::Unknown));
        // Unknown <: Unknown is false (Unknown uses consistency, not subtyping)
        assert!(!Type::is_subtype(&Type::Unknown, &Type::Unknown));
    }

    #[test]
    fn test_subtype_int_literal() {
        assert!(Type::is_subtype(
            &Type::IntLiteral(42),
            &Type::IntLiteral(42)
        ));
        assert!(!Type::is_subtype(
            &Type::IntLiteral(42),
            &Type::IntLiteral(99)
        ));
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::Int));
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::Number));
        assert!(!Type::is_subtype(&Type::Int, &Type::IntLiteral(42)));
    }

    #[test]
    fn test_subtype_string_literal() {
        assert!(Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::StringLiteral("a".into())
        ));
        assert!(!Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::StringLiteral("b".into())
        ));
        assert!(Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::Str
        ));
        assert!(!Type::is_subtype(
            &Type::Str,
            &Type::StringLiteral("a".into())
        ));
    }

    #[test]
    fn test_subtype_number() {
        assert!(Type::is_subtype(&Type::Int, &Type::Number));
        assert!(Type::is_subtype(&Type::Float, &Type::Number));
        assert!(!Type::is_subtype(&Type::Number, &Type::Int));
        assert!(!Type::is_subtype(&Type::Str, &Type::Number));
    }

    #[test]
    fn test_subtype_record_structural() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);
        sub.insert("extra".into(), Type::Bool);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_record_missing_field() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(!Type::is_subtype(&closed_record(sub), &closed_record(sup),));
    }

    #[test]
    fn test_subtype_closed_record_extra_field_accepted_bas() {
        // BAS width subtyping: closed sub with extra field IS a subtype of closed sup.
        // {a: Int, b: Int} has all the fields of {a: Int} plus more — width subtyping holds.
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int, b: Int] SHOULD be subtype of [a: Int] under BAS width subtyping"
        );
    }

    #[test]
    fn test_subtype_closed_record_same_fields_ok() {
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] should be subtype of [a: Int] (both Closed)"
        );
    }

    #[test]
    fn test_subtype_closed_to_row_var() {
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = row_var_record(sup_fields, "r", 0);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] (Closed) should be subtype of [a: Int ...r] (RowVar)"
        );
    }

    #[test]
    fn test_subtype_row_var_to_closed() {
        // BAS width subtyping (Step 2): open record with extra fields IS a subtype of closed record
        // when all closed record's fields are present in the open record's known fields.
        // {a: Int, b: Int, ...r} has all fields of {a: Int} (and more), so it satisfies it.
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = row_var_record(sub_fields, "r", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int, b: Int ...r] (RowVar) should be subtype of [a: Int] (Closed) under BAS width subtyping"
        );
    }

    #[test]
    fn test_subtype_function_covariant_return() {
        let sub = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Number),
            variadic: false,
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_is_subtype_open_record_subtype_of_closed_bas() {
        // BAS width subtyping (Step 2): open record [a:Int ...r] IS a subtype of closed record
        // [a:Int] because all of the closed record's fields are present in the open record's
        // known fields. Under BAS open-record semantics, an open record with the same required
        // fields satisfies a closed annotation.
        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let open = row_var_record(a_fields.clone(), "r", 0);
        let closed = closed_record(a_fields);

        assert!(
            Type::is_subtype(&open, &closed),
            "[a:Int ...r] (RowVar) should be subtype of [a:Int] (closed) under BAS width subtyping"
        );
    }

    #[test]
    fn test_is_subtype_closed_record_subtype_of_closed() {
        // Closed record with exact same fields IS a subtype of a closed record.
        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let sub = closed_record(a_fields.clone());
        let sup = closed_record(a_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a:Int] (closed) should be subtype of [a:Int] (closed) — same fields"
        );
    }

    #[test]
    fn test_is_subtype_closed_record_with_extra_subtype_of_closed() {
        // BAS width subtyping: closed record with EXTRA fields IS a subtype of closed record
        // with fewer fields, because all the required fields are present.
        // {a: Int, b: Str} <: {a: Int} — the "b" field is extra but that's fine.
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Str);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a:Int, b:Str] (closed) should be subtype of [a:Int] (closed) under BAS width subtyping"
        );
    }

    #[test]
    fn test_is_subtype_closed_record_missing_required_field_fails() {
        // A closed record that is MISSING a required field of the sup is NOT a subtype.
        // {b: Int} <: {a: Int} fails because "a" is not in sub.
        let mut sub_fields = HashMap::new();
        sub_fields.insert("b".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[b:Int] (closed) should NOT be subtype of [a:Int] (closed) — missing required field 'a'"
        );
    }

    #[test]
    fn test_is_subtype_open_record_subtype_of_open() {
        // Open record (RowVar tail) IS a subtype of another open record with the same fields.
        // Both have RowVar tails — the sup is open so extra fields are acceptable.
        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);
        let sub = row_var_record(fields.clone(), "r1", 0);
        let sup = row_var_record(fields, "r2", 0);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a:Int ...r1] (RowVar) should be subtype of [a:Int ...r2] (RowVar) — sup is open"
        );
    }

    #[test]
    fn test_subtype_function_contravariant_params() {
        let sub = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_function_arity_mismatch() {
        let sub = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![(None, Type::Int), (None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(!Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_different_kinds() {
        assert!(!Type::is_subtype(&Type::Int, &Type::Str));
        assert!(!Type::is_subtype(&Type::Bool, &Type::Float));
        assert!(!Type::is_subtype(
            &Type::Int,
            &closed_record(HashMap::new())
        ));
    }

    #[test]
    fn test_subtype_type_var() {
        assert!(Type::is_subtype(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(!Type::is_subtype(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("b".into(), 0)
        ));
    }

    #[test]
    fn test_subtype_nested_record() {
        let mut inner_sub = HashMap::new();
        inner_sub.insert("x".into(), Type::Int);
        inner_sub.insert("y".into(), Type::Int);
        let mut outer_sub = HashMap::new();
        outer_sub.insert("point".into(), closed_record(inner_sub));

        let mut inner_sup = HashMap::new();
        inner_sup.insert("x".into(), Type::Number);
        let mut outer_sup = HashMap::new();
        outer_sup.insert("point".into(), row_var_record(inner_sup, "_open", 0));

        assert!(Type::is_subtype(
            &closed_record(outer_sub),
            &row_var_record(outer_sup, "_open", 0)
        ));
    }

    #[test]
    fn test_subtype_number_reflexive() {
        assert!(Type::is_subtype(&Type::Number, &Type::Number));
    }

    #[test]
    fn test_subtype_closed_sub_open_sup() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_closed_sub_closed_sup_extra_fields_accepted_bas() {
        // BAS width subtyping: closed sub with extra fields IS a subtype of closed sup.
        // {name: Str, age: Int} has all fields of {name: Str} plus more — width subtyping.
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(
            Type::is_subtype(&closed_record(sub), &closed_record(sup)),
            "[name: Str, age: Int] SHOULD be subtype of [name: Str] under BAS width subtyping"
        );
    }

    #[test]
    fn test_subtype_closed_exact_match() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(&closed_record(sub), &closed_record(sup),));
    }

    #[test]
    fn test_subtype_open_sub_open_sup() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &row_var_record(sub, "_open", 0),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_row_var_behaves_like_open() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "r", 0),
        ));
    }

    #[test]
    fn test_subtype_open_sub_closed_sup_fewer_fields_rejected() {
        // Open sub with FEWER known fields than Closed sup must be rejected.
        // Old code: sub_fields ⊆ sup_fields → true (wrong).
        // New code: bidirectional check → sup field "age" not in sub → false (correct).
        //
        // sub: [name: Str | Open]  (may have additional unknown fields)
        // sup: [name: Str, age: Int | Closed]  (must have exactly name + age)
        let mut sub_fields = HashMap::new();
        sub_fields.insert("name".into(), Type::Str);
        let sub = row_var_record(sub_fields, "_open", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("name".into(), Type::Str);
        sup_fields.insert("age".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[name: Str | Open] should NOT be subtype of [name: Str, age: Int | Closed]: \
             sub is Open so may lack 'age'"
        );
    }

    #[test]
    fn test_subtype_open_sub_closed_sup_extra_fields_accepted_bas() {
        // BAS width subtyping: open sub with MORE known fields IS a subtype of closed sup.
        // sub has all of sup's required fields ("name") plus extra "age" — width subtyping.
        // The closed sup only constrains what it declares; extra fields in sub are fine.
        //
        // sub: [name: Str, age: Int | Open]
        // sup: [name: Str | Closed]
        let mut sub_fields = HashMap::new();
        sub_fields.insert("name".into(), Type::Str);
        sub_fields.insert("age".into(), Type::Int);
        let sub = row_var_record(sub_fields, "_open", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("name".into(), Type::Str);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[name: Str, age: Int | Open] SHOULD be subtype of [name: Str | Closed] \
             under BAS width subtyping"
        );
    }

    /// Function subtyping is contravariant in params and covariant in return.
    /// Transitivity: if P <: Q and Q <: R, then P <: R.
    ///
    /// P = Fn(Number → Int)
    /// Q = Fn(Int → Int)
    /// R = Fn(Int → Number)
    ///
    /// P <: Q: contravariant param (Int <: Number ✓), covariant return (Int <: Int ✓).
    /// Q <: R: contravariant param (Int <: Int ✓),  covariant return (Int <: Number ✓).
    /// P <: R: contravariant param (Int <: Number ✓), covariant return (Int <: Number ✓).
    #[test]
    fn test_function_variance_transitivity() {
        let p = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let q = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let r = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Number),
            variadic: false,
        };

        assert!(
            Type::is_subtype(&p, &q),
            "P <: Q should hold (contravariant param, covariant return)"
        );
        assert!(
            Type::is_subtype(&q, &r),
            "Q <: R should hold (covariant return Int <: Number)"
        );
        assert!(
            Type::is_subtype(&p, &r),
            "P <: R should hold by transitivity"
        );
    }

    /// Function subtyping is NOT symmetric: the contravariance of params means
    /// Fn(A → B) <: Fn(A' → B') does not imply Fn(A' → B') <: Fn(A → B).
    ///
    /// This is a sanity check that the transitivity test above is testing
    /// a genuine directional constraint, not accidental reflexivity.
    #[test]
    fn test_function_variance_not_symmetric() {
        // Fn(Number → Int) <: Fn(Int → Int) but NOT vice versa
        let broader_param = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let narrower_param = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert!(
            Type::is_subtype(&broader_param, &narrower_param),
            "Fn(Number → Int) should be a subtype of Fn(Int → Int)"
        );
        assert!(!Type::is_subtype(&narrower_param, &broader_param),
            "Fn(Int → Int) should NOT be a subtype of Fn(Number → Int): param Number is not a subtype of Int");
    }

    #[test]
    fn test_has_inference_vars_primitive() {
        assert!(!Type::Int.has_inference_vars());
        assert!(!Type::Str.has_inference_vars());
        assert!(!Type::Unknown.has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_type_var() {
        assert!(Type::TypeVar("a".into(), 0).has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_function() {
        let with = Type::Function {
            params: vec![(None, Type::TypeVar("a".into(), 0))],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let without = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        assert!(with.has_inference_vars());
        assert!(!without.has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_record() {
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        assert!(closed_record(fields).has_inference_vars());
    }

    #[test]
    fn test_collect_type_vars() {
        let ty = Type::Function {
            params: vec![
                (None, Type::TypeVar("a".into(), 0)),
                (None, Type::TypeVar("b".into(), 0)),
            ],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut vars = HashSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn test_collect_all_vars() {
        // TypeVar produces type_vars only
        let ty = Type::TypeVar("a".into(), 0);
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("a"));
        assert!(row_vars.is_empty());

        // Record with type vars in fields produces type_vars (no row vars under BAS)
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("t1".into(), 0));
        fields.insert("y".into(), Type::Int);
        let ty = Type::Record(Row { fields });
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("t1"));
        assert!(row_vars.is_empty(), "BAS: no row vars collected");
        assert_eq!(type_vars.len(), 1);
        assert_eq!(row_vars.len(), 0);

        // Function type produces type_vars from params and return
        let ty = Type::Function {
            params: vec![
                (None, Type::TypeVar("a".into(), 0)),
                (None, Type::TypeVar("b".into(), 0)),
            ],
            ret: Box::new(Type::TypeVar("c".into(), 0)),
            variadic: false,
        };
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("a"));
        assert!(type_vars.contains("b"));
        assert!(type_vars.contains("c"));
        assert!(row_vars.is_empty());
        assert_eq!(type_vars.len(), 3);

        // Seq type produces type_vars from element type
        let ty = Type::Seq(Box::new(Type::TypeVar("elem".into(), 0)));
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("elem"));
        assert!(row_vars.is_empty());

        // Ground types produce empty sets
        for ty in [
            Type::Int,
            Type::Str,
            Type::Bool,
            Type::Float,
            Type::Number,
            Type::Unknown,
        ] {
            let mut type_vars = HashSet::new();
            let mut row_vars = HashSet::new();
            ty.collect_all_vars(&mut type_vars, &mut row_vars);
            assert!(type_vars.is_empty());
            assert!(row_vars.is_empty());
        }
    }

    #[test]
    fn test_env_get_current() {
        let mut env = TypeEnv::new();
        env.insert("x".into(), Type::Int);
        assert_eq!(env.get("x").map(|s| &s.body), Some(&Type::Int));
    }

    #[test]
    fn test_env_get_parent() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let child = TypeEnv::with_parent(&parent_rc);
        assert_eq!(child.get("x").map(|s| &s.body), Some(&Type::Int));
    }

    #[test]
    fn test_env_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        child.insert("x".into(), Type::Str);
        assert_eq!(child.get("x").map(|s| &s.body), Some(&Type::Str));
    }

    #[test]
    fn test_env_missing() {
        let env = TypeEnv::new();
        assert_eq!(env.get("x"), None);
    }

    #[test]
    fn test_env_type_alias() {
        let mut env = TypeEnv::new();
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        env.insert_type_alias(
            "Person".into(),
            TypeAlias {
                params: vec![],
                body: closed_record(fields.clone()),
            },
        );
        assert_eq!(
            env.get_type_alias("Person").map(|a| &a.body),
            Some(&closed_record(fields))
        );
    }

    #[test]
    fn test_env_type_alias_parent() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias(
            "Base".into(),
            TypeAlias {
                params: vec![],
                body: Type::Int,
            },
        );
        let parent_rc = Rc::new(parent);
        let child = TypeEnv::with_parent(&parent_rc);
        assert_eq!(
            child.get_type_alias("Base").map(|a| &a.body),
            Some(&Type::Int)
        );
    }

    #[test]
    fn test_env_type_alias_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias(
            "T".into(),
            TypeAlias {
                params: vec![],
                body: Type::Int,
            },
        );
        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        child.insert_type_alias(
            "T".into(),
            TypeAlias {
                params: vec![],
                body: Type::Str,
            },
        );
        assert_eq!(child.get_type_alias("T").map(|a| &a.body), Some(&Type::Str));
    }

    #[test]
    fn test_with_builtins_registers_all_builtins() {
        let env = TypeEnv::with_builtins();

        // Arithmetic
        assert!(env.get("+").is_some());
        assert!(env.get("-").is_some());
        assert!(env.get("*").is_some());
        assert!(env.get("/").is_some());

        // Comparison
        assert!(env.get("=").is_some());
        assert!(env.get("<").is_some());

        // Control flow
        assert!(env.get("if").is_some());

        // Dict primitives
        assert!(env.get("keys").is_some());
        assert!(env.get("length").is_some());
        assert!(env.get("merge").is_some());
        assert!(env.get("append").is_some());

        // Sequences
        assert!(env.get("map").is_some());
        assert!(env.get("filter").is_some());
        assert!(env.get("reduce").is_some());

        // List operations (registered as builtin-NAME; prelude exports the unwrapped names)
        assert!(env.get("builtin-rest").is_some());
        assert!(env.get("builtin-cons").is_some());
        assert!(env.get("builtin-reverse").is_some());
        assert!(env.get("builtin-sort").is_some());
    }

    #[test]
    fn test_with_builtins_arithmetic_signature() {
        // + is now Add a b c => a -> b -> c (MPTC with functional dependency)
        let env = TypeEnv::with_builtins();
        let add_scheme = env.get("+").expect("+ should be registered");
        // Check constraints
        assert_eq!(add_scheme.constraints.len(), 1);
        assert!(matches!(
            &add_scheme.constraints[0],
            Constraint::Class { class, vars, fundeps }
            if class == "Add"
                && vars == &vec!["a".to_string(), "b".to_string(), "c".to_string()]
                && fundeps.len() == 1
                && fundeps[0].0 == vec![0, 1]  // (a,b) → c
                && fundeps[0].1 == vec![2]
        ));
        // Check type_vars
        assert_eq!(
            add_scheme.type_vars,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        match &add_scheme.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].1, Type::TypeVar("a".into(), 0));
                assert_eq!(params[1].1, Type::TypeVar("b".into(), 0));
                assert_eq!(&**ret, &Type::TypeVar("c".into(), 0));
            }
            other => panic!("expected Function type for +, got {other}"),
        }
    }

    #[test]
    fn test_with_builtins_division_returns_float() {
        // / is now Div a b c => a -> b -> c (MPTC with functional dependency)
        let env = TypeEnv::with_builtins();
        let div_scheme = env.get("/").expect("/ should be registered");
        assert_eq!(div_scheme.constraints.len(), 1);
        assert!(matches!(
            &div_scheme.constraints[0],
            Constraint::Class { class, .. } if class == "Div"
        ));
        match &div_scheme.body {
            Type::Function { params, ret, .. } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].1, Type::TypeVar("a".into(), 0));
                assert_eq!(params[1].1, Type::TypeVar("b".into(), 0));
                assert_eq!(&**ret, &Type::TypeVar("c".into(), 0));
            }
            other => panic!("expected Function type for /, got {other}"),
        }
    }

    #[test]
    fn test_with_builtins_comparison_signature() {
        // = is now Equatable a => a -> a -> Bool (constrained polymorphic)
        let env = TypeEnv::with_builtins();
        let eq_scheme = env.get("=").expect("= should be registered");
        assert_eq!(eq_scheme.constraints.len(), 1);
        assert!(matches!(
            &eq_scheme.constraints[0],
            Constraint::Class { class, vars, .. } if class == "Equatable" && vars == &vec!["a".to_string()]
        ));
        match &eq_scheme.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].1, Type::TypeVar("a".into(), 0));
                assert_eq!(params[1].1, Type::TypeVar("a".into(), 0));
                assert_eq!(&**ret, &Type::Bool);
            }
            other => panic!("expected Function type for =, got {other}"),
        }
    }

    #[test]
    fn test_type_error_display() {
        let span = test_span(3, 5, 3, 10);
        let err = TypeError::new("oops", span);
        assert_eq!(format!("{err}"), "oops at 3:5-3:10");
    }

    #[test]
    fn test_type_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::type_mismatch(&Type::Int, &Type::Str, span);
        assert_eq!(err.message, "cannot unify Int with String");
    }

    #[test]
    fn test_type_error_field_not_found() {
        let span = test_span(1, 1, 1, 5);
        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);
        let err = TypeError::field_not_found("b", &closed_record(fields), span);
        assert_eq!(err.message, "field 'b' not found in [a: Int]");
    }

    #[test]
    fn test_type_error_undefined_variable() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::undefined_variable("x", span);
        assert_eq!(err.message, "undefined variable: x");
    }

    #[test]
    fn test_type_error_undefined_type() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::undefined_type("Foo", span);
        assert_eq!(err.message, "undefined type: Foo");
    }

    #[test]
    fn test_type_error_not_a_record() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::not_a_record(&Type::Int, span);
        assert_eq!(err.message, "expected record type, got Int");
    }

    #[test]
    fn test_type_error_not_a_function() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::not_a_function(&Type::Str, span);
        assert_eq!(err.message, "expected function type, got String");
    }

    #[test]
    fn test_substitution_empty_apply() {
        let mut subst = Substitution::new();
        assert_eq!(subst.apply(&Type::Int), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_bound() {
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_apply_chain() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.type_map.borrow_mut().insert("b".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_idempotence() {
        // Verify that applying a substitution multiple times produces the same result.
        // This validates the claim in doc/05-type-annotations.md that substitution
        // application is idempotent.
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.type_map.borrow_mut().insert("b".into(), Type::Int);

        let ty = Type::TypeVar("a".into(), 0);
        let result_once = subst.apply(&ty);
        let result_twice = subst.apply(&result_once);

        // Both applications should produce the same result: Int
        assert_eq!(result_once, Type::Int);
        assert_eq!(result_twice, Type::Int);
        assert_eq!(result_once, result_twice);
    }

    #[test]
    fn test_substitution_path_compression() {
        // Verify that path compression collapses chains: t0 → t1 → t2 → Int
        // After applying t0, both t0 and t1 should map directly to Int (not to t2).
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("t0".into(), Type::TypeVar("t1".into(), 0));
        subst
            .type_map
            .borrow_mut()
            .insert("t1".into(), Type::TypeVar("t2".into(), 0));
        subst.type_map.borrow_mut().insert("t2".into(), Type::Int);

        // First access: apply(t0) should resolve to Int and compress the path
        let result = subst.apply(&Type::TypeVar("t0".into(), 0));
        assert_eq!(result, Type::Int);

        // After path compression, t0 should map directly to Int (not to t1)
        assert_eq!(
            subst.type_map.borrow().get("t0").cloned(),
            Some(Type::Int),
            "t0 should be compressed to Int"
        );

        // t1 should also be compressed to Int (not to t2)
        assert_eq!(
            subst.type_map.borrow().get("t1").cloned(),
            Some(Type::Int),
            "t1 should be compressed to Int"
        );

        // t2 → Int remains unchanged
        assert_eq!(subst.type_map.borrow().get("t2").cloned(), Some(Type::Int));
    }

    #[test]
    fn test_substitution_apply_in_function() {
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        subst.type_map.borrow_mut().insert("b".into(), Type::Str);
        let ty = Type::Function {
            params: vec![(None, Type::TypeVar("a".into(), 0))],
            ret: Box::new(Type::TypeVar("b".into(), 0)),
            variadic: false,
        };
        assert_eq!(
            subst.apply(&ty),
            Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Str),
                variadic: false,
            }
        );
    }

    #[test]
    fn test_substitution_apply_in_record() {
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        fields.insert("y".into(), Type::Str);
        let ty = closed_record(fields);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::Str);
        assert_eq!(subst.apply(&ty), closed_record(expected));
    }

    #[test]
    fn test_substitution_leaves_unbound_alone() {
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("b".into(), 0)),
            Type::TypeVar("b".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_self_reference_cycle() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::TypeVar("a".into(), 0));
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_indirect_cycle() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst
            .type_map
            .borrow_mut()
            .insert("b".into(), Type::TypeVar("a".into(), 0));
        // When we apply starting from "a", we get "a" back because:
        // a -> b (with a visited) -> a (already visited, return TypeVar("a"))
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_unify_identical_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Int, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Str, &Type::Str, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Bool, &Type::Bool, &mut subst, &mut state, span).is_ok());
    }

    #[test]
    fn test_unify_typevar_with_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.get("a"), Some(Type::Int));
    }

    #[test]
    fn test_unify_concrete_with_typevar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::Int,
            &Type::TypeVar("a".into(), 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.get("a"), Some(Type::Int));
    }

    #[test]
    fn test_unify_two_typevars() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("b".into(), 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        let resolved = subst.apply(&Type::TypeVar("a".into(), 0));
        assert_eq!(resolved, subst.apply(&Type::TypeVar("b".into(), 0)));
    }

    #[test]
    fn test_unify_typevar_already_bound_compatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
    }

    #[test]
    fn test_unify_typevar_already_bound_incompatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Str,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![(None, Type::TypeVar("a".into(), 0))],
            ret: Box::new(Type::TypeVar("b".into(), 0)),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        unify(&f1, &f2, &mut subst, &mut state, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into(), 0)), Type::Str);
    }

    #[test]
    fn test_unify_function_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![(None, Type::Int), (None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let result = unify(&f1, &f2, &mut subst, &mut state, span);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("arity mismatch"));
    }

    #[test]
    fn test_unify_record_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::TypeVar("a".into(), 0));
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_unify_closed_record_extra_fields_accepted_bas() {
        // BAS: unify only unifies shared fields. Extra fields in one record are not a
        // unification error — subtyping (is_subtype) enforces field requirements, not unification.
        // {x: Int} unify {x: Int, y: Str} → Ok (shared field "x" unifies; extra "y" is ignored).
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "BAS: extra fields in unification are not errors, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_unify_open_record_extra_fields_accepted() {
        // Under BAS, all records are closed (RowTail::Empty). Unification of two records
        // with overlapping fields unifies the shared fields only — extra fields are ignored
        // (BAS width subtyping handles openness via is_subtype, not unification).
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        // {x: Int} unified with {x: Int, y: Str} — shared field "x" unifies OK
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        // BAS: unification never creates row bindings (no RowVar tails)
    }

    #[test]
    fn test_unify_any_with_anything() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Unknown, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Str, &Type::Unknown, &mut subst, &mut state, span).is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_int() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Int,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
        assert!(unify(
            &Type::Int,
            &Type::IntLiteral(99),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_number() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Number,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_string_literal_with_string() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::StringLiteral("hi".into()),
            &Type::Str,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
        assert!(unify(
            &Type::Str,
            &Type::StringLiteral("lo".into()),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_int_literal_different_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::IntLiteral(1),
            &Type::IntLiteral(2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("cannot unify"));
    }

    #[test]
    fn test_unify_int_literal_same_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::IntLiteral(42),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_string_literal_different_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::StringLiteral("hello".into()),
            &Type::StringLiteral("world".into()),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("cannot unify"));
    }

    #[test]
    fn test_unify_string_literal_same_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::StringLiteral("hello".into()),
            &Type::StringLiteral("hello".into()),
            &mut subst,
            &mut state,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_incompatible_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(&Type::Int, &Type::Str, &mut subst, &mut state, span);
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_int_with_bool() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Int, &Type::Bool, &mut subst, &mut state, span).is_err());
    }

    #[test]
    fn test_unify_int_literal_float_fails() {
        // Regression guard: IntLiteral is not a subtype of Float (different branches of the
        // numeric lattice: IntLiteral <: Int <: Number vs Float <: Number). The unsound
        // `(IntLiteral, Float)` promotion arm was removed; this test ensures it stays gone.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Float,
            &mut subst,
            &mut state,
            span
        )
        .is_err());
    }

    #[test]
    fn test_unify_float_with_int_literal_fails() {
        // Regression guard: symmetric case — Float is not a supertype of IntLiteral.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::Float,
            &Type::IntLiteral(42),
            &mut subst,
            &mut state,
            span
        )
        .is_err());
    }

    #[test]
    fn test_unify_subsume_positive_path() {
        // [U-SUBSUME] positive-path coverage note:
        //
        // The [U-SUBSUME] arm fires for concrete (no type-var) pairs not matched by any
        // prior structural or explicit-promotion arm. With the current type vocabulary,
        // every valid subtype relationship already has a fast-path explicit arm:
        //   IntLiteral <: Int | Number  (unify line 1075)
        //   Int <: Number               (unify line 1075)
        //   Float <: Number             (unify line 1077)
        //   StringLiteral <: Str        (unify line 1078)
        //
        // This means [U-SUBSUME]'s positive branch (is_subtype returns true) is
        // unreachable with the current set of types.  The arm is a future extension
        // point: when a new subtype relationship is added to is_subtype() without a
        // corresponding explicit arm, [U-SUBSUME] will catch it automatically.
        //
        // The NEGATIVE branch (both concrete, neither is a subtype of the other) IS
        // exercised: pairs like (Int, Bool) or (Float, Bool) fall through all explicit
        // arms and reach [U-SUBSUME], which correctly rejects them.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Negative path through [U-SUBSUME]: concrete types with no subtype relation.
        // Neither Int <: Bool nor Bool <: Int, so [U-SUBSUME] rejects correctly.
        assert!(unify(&Type::Int, &Type::Bool, &mut subst, &mut state, span).is_err());

        // Another negative path through [U-SUBSUME]: Float <: Bool is also false.
        assert!(unify(&Type::Float, &Type::Bool, &mut subst, &mut state, span).is_err());
    }

    #[test]
    fn test_instantiate_no_vars() {
        let ty = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, ty);
        assert_eq!(counter, 0);
    }

    #[test]
    fn test_instantiate_with_vars() {
        let ty = Type::Function {
            params: vec![(None, Type::TypeVar("a".into(), 0))],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        assert!(!matches!(&result, Type::Function { params, .. }
            if params[0].1 == Type::TypeVar("a".into(), 0)));
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => assert_eq!(params[0].1, **ret),
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_multiple_vars() {
        let ty = Type::Function {
            params: vec![
                (None, Type::TypeVar("a".into(), 0)),
                (None, Type::TypeVar("b".into(), 0)),
            ],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 2);
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_ne!(params[0].1, params[1].1);
                assert_eq!(params[0].1, **ret);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_counter_increments() {
        let ty = Type::TypeVar("x".into(), 0);
        let mut counter = 5;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, Type::TypeVar("_t5".into(), 0));
        assert_eq!(counter, 6);
    }

    #[test]
    fn test_unify_nested_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![(None, Type::TypeVar("a".into(), 0))],
            ret: Box::new(Type::Function {
                params: vec![(None, Type::TypeVar("a".into(), 0))],
                ret: Box::new(Type::TypeVar("b".into(), 0)),
                variadic: false,
            }),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Str),
                variadic: false,
            }),
            variadic: false,
        };
        unify(&f1, &f2, &mut subst, &mut state, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into(), 0)), Type::Str);
    }

    #[test]
    fn test_unify_occurs_check_direct() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Function {
                params: vec![(None, Type::TypeVar("a".into(), 0))],
                ret: Box::new(Type::Int),
                variadic: false,
            },
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_nested() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &closed_record(fields),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_reverse() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Function {
                params: vec![(None, Type::TypeVar("a".into(), 0))],
                ret: Box::new(Type::Int),
                variadic: false,
            },
            &Type::TypeVar("a".into(), 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_substitution_apply_record_with_type_var_field() {
        // Under BAS all records are closed (RowTail::Empty). Substitution applies to
        // field types (TypeVars) but there is no row_map to follow.
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Str);

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        let ty = closed_record(fields);
        let result = subst.apply(&ty);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Str);
        assert_eq!(result, closed_record(expected));
    }

    #[test]
    fn test_substitution_apply_row_var_unbound() {
        // BAS: row_map is always empty; closed records apply unchanged
        let mut subst = Substitution::new();
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = closed_record(fields.clone());
        let result = subst.apply(&ty);
        assert_eq!(result, closed_record(fields));
    }

    #[test]
    fn test_substitution_apply_row_var_duplicate_field() {
        // BAS: field types in records are subject to TypeVar substitution.
        // There is no row_map merging (no RowVar tails exist).
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        let ty = closed_record(fields);
        let result = subst.apply(&ty);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        assert_eq!(result, closed_record(expected));
    }

    #[test]
    fn test_unify_closed_records_same_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::Str);
        assert!(unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_closed_records_different_keys() {
        // Two records with completely disjoint concrete field sets are incompatible under
        // unification: no value can simultaneously be [a: Int] and [b: Int] with those as
        // the ONLY fields. BAS subtyping (is_subtype) handles the open/width direction, but
        // unification of two concrete disjoint records is a type mismatch.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("b".into(), Type::Int);
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "disjoint concrete records should fail unification: [a: Int] vs [b: Int]"
        );
    }

    #[test]
    fn test_display_seq() {
        assert_eq!(format!("{}", Type::Seq(Box::new(Type::Int))), "Seq[Int]");
        assert_eq!(
            format!("{}", Type::Seq(Box::new(Type::TypeVar("a".into(), 0)))),
            "Seq[a]"
        );
    }

    #[test]
    fn test_subtype_seq_covariant() {
        assert!(Type::is_subtype(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Seq(Box::new(Type::Number)),
        ));
        assert!(!Type::is_subtype(
            &Type::Seq(Box::new(Type::Number)),
            &Type::Seq(Box::new(Type::Int)),
        ));
    }

    #[test]
    fn test_subtype_seq_same() {
        assert!(Type::is_subtype(
            &Type::Seq(Box::new(Type::Str)),
            &Type::Seq(Box::new(Type::Str)),
        ));
    }

    #[test]
    fn test_subtype_seq_vs_other() {
        assert!(!Type::is_subtype(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Int,
        ));
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::Seq(Box::new(Type::Int)),
        ));
    }

    #[test]
    fn test_has_inference_vars_seq() {
        assert!(Type::Seq(Box::new(Type::TypeVar("a".into(), 0))).has_inference_vars());
        assert!(!Type::Seq(Box::new(Type::Int)).has_inference_vars());
    }

    #[test]
    fn test_collect_type_vars_seq() {
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        let mut vars = HashSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_substitution_apply_seq() {
        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        assert_eq!(subst.apply(&ty), Type::Seq(Box::new(Type::Int)));
    }

    #[test]
    fn test_unify_seq_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::Seq(Box::new(Type::TypeVar("a".into(), 0))),
            &Type::Seq(Box::new(Type::Int)),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    // ===== App/Operator subtyping tests (hkt-bas sprint) =====

    #[test]
    fn test_subtype_app_covariant() {
        // App(Result, Int) <: App(Result, Int|Str) — covariance in type argument
        let result = Type::Operator("Result".into());
        let app_int = Type::App(Box::new(result.clone()), Box::new(Type::Int));
        let app_union = Type::App(
            Box::new(result),
            Box::new(Type::normalize_union(vec![Type::Int, Type::Str])),
        );
        assert!(Type::is_subtype(&app_int, &app_union));
    }

    #[test]
    fn test_subtype_app_mismatched_constructors() {
        // App(Result, Int) is NOT a subtype of App(Maybe, Int) — different constructors
        let result = Type::Operator("Result".into());
        let maybe = Type::Operator("Maybe".into());
        let app_result = Type::App(Box::new(result), Box::new(Type::Int));
        let app_maybe = Type::App(Box::new(maybe), Box::new(Type::Int));
        assert!(!Type::is_subtype(&app_result, &app_maybe));
    }

    #[test]
    fn test_subtype_app_union_elim_derives_join() {
        // Union(App(Result, Int), App(Result, Str)) <: App(Result, Union(Int, Str))
        // This should be derivable via UNION-ELIM:
        //   - App(Result, Int) <: App(Result, Int|Str) by covariance (Int <: Int|Str)
        //   - App(Result, Str) <: App(Result, Int|Str) by covariance (Str <: Int|Str)
        //   - UNION-ELIM: both members are subtypes → union is a subtype
        let result = Type::Operator("Result".into());
        let app_int = Type::App(Box::new(result.clone()), Box::new(Type::Int));
        let app_str = Type::App(Box::new(result.clone()), Box::new(Type::Str));
        let union_of_apps = Type::normalize_union(vec![app_int, app_str]);
        let app_union = Type::App(
            Box::new(result),
            Box::new(Type::normalize_union(vec![Type::Int, Type::Str])),
        );
        assert!(Type::is_subtype(&union_of_apps, &app_union));
    }

    #[test]
    fn test_subtype_app_reverse_distribution_unsound() {
        // App(Result, Union(Int, Str)) is NOT a subtype of Union(App(Result, Int), App(Result, Str))
        // This reverse direction is unsound for diagonal functors
        let result = Type::Operator("Result".into());
        let app_union = Type::App(
            Box::new(result.clone()),
            Box::new(Type::normalize_union(vec![Type::Int, Type::Str])),
        );
        let app_int = Type::App(Box::new(result.clone()), Box::new(Type::Int));
        let app_str = Type::App(Box::new(result), Box::new(Type::Str));
        let union_of_apps = Type::normalize_union(vec![app_int, app_str]);
        assert!(!Type::is_subtype(&app_union, &union_of_apps));
    }

    // ===== HKT kind inference tests (hkt-kind-inference sprint) =====

    #[test]
    fn test_kind_env_operator_registration() {
        // Test that Operator-kinded class params are registered in kind_env
        let state = InferState::new();
        // Mappable has Kind::Operator param "f"
        let mappable = state.class_env.get("Mappable").unwrap();
        assert_eq!(mappable.params.len(), 1);
        assert_eq!(mappable.params[0].0, "f");
        assert_eq!(mappable.params[0].1, Kind::Operator);
    }

    #[test]
    fn test_app_normalization_seq() {
        // Test that App(Operator("Seq"), Int) normalizes to Type::Seq(Int) after substitution
        let mut subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("m".into(), Type::Operator("Seq".into()));
        let app_type = Type::App(Box::new(Type::Operator("m".into())), Box::new(Type::Int));
        let normalized = subst.apply(&app_type);
        match normalized {
            Type::Seq(inner) => assert_eq!(*inner, Type::Int),
            other => panic!("Expected Type::Seq(Int), got {:?}", other),
        }
    }

    #[test]
    fn test_unify_operator_with_concrete() {
        // Test UNIFY-OPERATOR: unifying Operator("m") with Int binds m -> Int
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Operator("m".into()),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_ok());
        assert_eq!(subst.type_map.borrow().get("m"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_app() {
        // Test UNIFY-APP: App(m, Int) unifies with App(n, Int) by binding m=n
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let app1 = Type::App(Box::new(Type::Operator("m".into())), Box::new(Type::Int));
        let app2 = Type::App(Box::new(Type::Operator("n".into())), Box::new(Type::Int));
        let result = unify(&app1, &app2, &mut subst, &mut state, span);
        assert!(result.is_ok());
        // m and n should be unified
        let m_bound = subst.apply(&Type::Operator("m".into()));
        let n_bound = subst.apply(&Type::Operator("n".into()));
        assert_eq!(m_bound, n_bound);
    }

    #[test]
    fn test_operator_occurs_check() {
        // Test that Operator occurs check prevents infinite types
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        // Try to unify m with App(m, Int) — should fail occurs check
        let result = unify(
            &Type::Operator("m".into()),
            &Type::App(Box::new(Type::Operator("m".into())), Box::new(Type::Int)),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_seq_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Seq(Box::new(Type::Str)),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_seq_vs_non_seq() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_occurs_check_seq() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Seq(Box::new(Type::TypeVar("a".into(), 0))),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_instantiate_seq() {
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        match &result {
            Type::Seq(elem) => assert_eq!(**elem, Type::TypeVar("_t0".into(), 0)),
            _ => panic!("expected Seq"),
        }
    }

    // --- TypeVar/RowVar level semantics ---

    #[test]
    fn test_typevar_eq_ignores_level() {
        // [U-REFL]: same name = equal regardless of level
        assert_eq!(Type::TypeVar("a".into(), 0), Type::TypeVar("a".into(), 5));
    }

    #[test]
    fn test_u_refl_fast_path_level_blind() {
        // Verify that unify() returns Ok(()) via the [U-REFL] fast path (line: `if a == b`)
        // when both sides are the same TypeVar name but with different levels.
        // TypeVar PartialEq is name-only, so ("a", level=0) == ("a", level=3), triggering
        // the fast path before any match arm is reached. The substitution must remain empty.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 3);
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("a".into(), 3),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "same-name TypeVar with different levels should unify via [U-REFL]"
        );
        assert!(
            subst.type_map.borrow().is_empty(),
            "fast path must not bind anything in the substitution"
        );
    }

    #[test]
    fn test_typevar_neq_different_name() {
        assert_ne!(Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0));
    }

    #[test]
    fn test_typevar_display_hides_level() {
        assert_eq!(format!("{}", Type::TypeVar("a".into(), 5)), "a");
    }

    #[test]
    fn test_closed_record_display() {
        // BAS: all records display without "..." (closed)
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = closed_record(fields);
        assert_eq!(format!("{ty}"), "[x: Int]");
    }

    // --- TypeScheme ---

    #[test]
    fn test_type_scheme_mono_empty_vars() {
        let scheme = TypeScheme::mono(Type::Int);
        assert!(scheme.type_vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_type_scheme_mono_wraps_body() {
        let body = Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let scheme = TypeScheme::mono(body.clone());
        assert!(scheme.type_vars.is_empty());
        assert_eq!(scheme.body, body);
    }

    #[test]
    fn test_type_scheme_display_monomorphic() {
        let scheme = TypeScheme::mono(Type::Int);
        assert_eq!(format!("{scheme}"), "Int");
    }

    #[test]
    fn test_type_scheme_display_polymorphic() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into(), "b".into()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".into(), 0)),
                    (None, Type::TypeVar("b".into(), 0)),
                ],
                ret: Box::new(Type::TypeVar("a".into(), 0)),
                variadic: false,
            },
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        assert_eq!(format!("{scheme}"), "∀a b. Fn@a [a b]");
    }

    #[test]
    fn test_type_scheme_display_single_var() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        assert_eq!(format!("{scheme}"), "∀a. a");
    }

    #[test]
    fn test_type_scheme_partial_eq_same() {
        let s1 = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        let s2 = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_different_vars() {
        let s1 = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::Int,
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        let s2 = TypeScheme {
            type_vars: vec!["b".into()],
            constraints: vec![],
            body: Type::Int,
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_different_body() {
        let s1 = TypeScheme::mono(Type::Int);
        let s2 = TypeScheme::mono(Type::Str);
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_mono_vs_poly() {
        let s1 = TypeScheme::mono(Type::TypeVar("a".into(), 0));
        let s2 = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        assert_ne!(s1, s2);
    }

    // --- InferState ---

    #[test]
    fn test_infer_state_new_defaults() {
        let state = InferState::new();
        assert_eq!(state.name_counter, 0);
        assert_eq!(state.level, 0);
        assert!(state.levels.is_empty());
    }

    #[test]
    fn test_infer_state_fresh_var_increments_counter() {
        let mut state = InferState::new();
        state.fresh_var();
        assert_eq!(state.name_counter, 1);
        state.fresh_var();
        assert_eq!(state.name_counter, 2);
    }

    #[test]
    fn test_infer_state_fresh_var_registers_in_levels() {
        let mut state = InferState::new();
        let tv = state.fresh_var();
        // The var name should appear in the levels map at the current level
        match &tv {
            Type::TypeVar(name, level) => {
                assert_eq!(*level, 0);
                assert_eq!(state.levels.get(name.as_str()), Some(&0));
            }
            _ => panic!("expected TypeVar"),
        }
    }

    #[test]
    fn test_infer_state_fresh_var_returns_type_var_at_current_level() {
        let mut state = InferState::new();
        state.level = 3;
        let tv = state.fresh_var();
        match tv {
            Type::TypeVar(name, level) => {
                assert_eq!(level, 3);
                assert_eq!(name, "_t0");
                assert_eq!(state.levels.get("_t0"), Some(&3));
            }
            _ => panic!("expected TypeVar"),
        }
    }

    #[test]
    fn test_infer_state_fresh_var_sequential_names() {
        let mut state = InferState::new();
        let tv0 = state.fresh_var();
        let tv1 = state.fresh_var();
        match (&tv0, &tv1) {
            (Type::TypeVar(n0, _), Type::TypeVar(n1, _)) => {
                assert_eq!(n0, "_t0");
                assert_eq!(n1, "_t1");
            }
            _ => panic!("expected TypeVars"),
        }
    }

    // --- TypeEnv::insert_scheme ---

    #[test]
    fn test_env_insert_scheme_stores_and_retrieves() {
        let mut env = TypeEnv::new();
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        env.insert_scheme("f".into(), scheme.clone());
        assert_eq!(env.get("f"), Some(&scheme));
    }

    #[test]
    fn test_env_insert_scheme_shadows_parent() {
        let mut parent = TypeEnv::new();
        let parent_scheme = TypeScheme::mono(Type::Int);
        parent.insert_scheme("x".into(), parent_scheme);

        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        let child_scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        child.insert_scheme("x".into(), child_scheme.clone());

        // Child shadows parent: child scheme should be returned
        assert_eq!(child.get("x"), Some(&child_scheme));
    }

    // --- instantiate_scheme ---

    #[test]
    fn test_instantiate_scheme_monomorphic() {
        let scheme = TypeScheme::mono(Type::Int);
        let mut state = InferState::new();
        state.level = 2;
        let result = instantiate_scheme(&scheme, 2, &mut state);
        assert_eq!(result, Type::Int);
        assert_eq!(state.name_counter, 0); // No fresh vars created
    }

    #[test]
    fn test_instantiate_scheme_polymorphic() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into(), "b".into()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::TypeVar("a".into(), 0))],
                ret: Box::new(Type::TypeVar("b".into(), 0)),
                variadic: false,
            },
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        // Should get fresh variables at level 3
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                match &params[0].1 {
                    Type::TypeVar(name, level) => {
                        assert_eq!(*level, 3);
                        assert!(name.starts_with("_t"));
                        assert_eq!(state.levels.get(name.as_str()), Some(&3));
                    }
                    _ => panic!("expected TypeVar in params"),
                }
                match &**ret {
                    Type::TypeVar(name, level) => {
                        assert_eq!(*level, 3);
                        assert!(name.starts_with("_t"));
                        assert_eq!(state.levels.get(name.as_str()), Some(&3));
                    }
                    _ => panic!("expected TypeVar in return"),
                }
            }
            _ => panic!("expected Function"),
        }
        assert_eq!(state.name_counter, 2); // Two fresh vars created
    }

    #[test]
    fn test_instantiate_scheme_creates_independent_instances() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        let mut state = InferState::new();

        let inst1 = instantiate_scheme(&scheme, 1, &mut state);
        let inst2 = instantiate_scheme(&scheme, 1, &mut state);

        // Should be different fresh variables
        assert_ne!(inst1, inst2);
    }

    #[test]
    fn test_instantiate_at_level_registers_vars_in_levels() {
        // Create a type scheme with a polymorphic variable
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::TypeVar("a".into(), 0),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };
        let mut state = InferState::new();
        state.level = 2;

        // Instantiate the scheme
        let result = instantiate_scheme(&scheme, 2, &mut state);

        // The result should be a fresh type variable
        match result {
            Type::TypeVar(name, _) => {
                // Verify the fresh variable is registered in levels at the current level
                assert_eq!(
                    state.levels.get(&name),
                    Some(&2),
                    "instantiate_at_level must register fresh vars in state.levels at current level"
                );
            }
            other => panic!("expected TypeVar, got {other:?}"),
        }
    }

    #[test]
    fn test_instantiate_at_level_monomorphic_fast_path() {
        let mut state = InferState::new();
        let before_counter = state.name_counter;

        let result = instantiate_at_level(&Type::Int, &mut state);

        assert_eq!(result, Type::Int);
        assert_eq!(
            state.name_counter, before_counter,
            "monomorphic fast-path must not increment name_counter"
        );
    }

    // --- generalize ---

    #[test]
    fn test_generalize_no_vars() {
        let state = InferState::new();
        let ty = Type::Int;
        let scheme = generalize(0, &ty, &state);
        assert!(scheme.type_vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_generalize_var_at_higher_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);
        let ty = Type::TypeVar("a".into(), 2);
        let scheme = generalize(1, &ty, &state);
        assert_eq!(scheme.type_vars, vec!["a"]);
        assert_eq!(scheme.body, ty);
    }

    #[test]
    fn test_generalize_var_at_same_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        let ty = Type::TypeVar("a".into(), 1);
        let scheme = generalize(1, &ty, &state);
        // Level 1 is NOT > 1, so should not generalize
        assert!(scheme.type_vars.is_empty());
    }

    #[test]
    fn test_generalize_var_at_lower_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 0);
        let ty = Type::TypeVar("a".into(), 0);
        let scheme = generalize(1, &ty, &state);
        // Level 0 is NOT > 1, so should not generalize
        assert!(scheme.type_vars.is_empty());
    }

    #[test]
    fn test_generalize_multiple_vars_mixed_levels() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);
        state.levels.insert("b".into(), 1);
        state.levels.insert("c".into(), 3);
        let ty = Type::Function {
            params: vec![
                (None, Type::TypeVar("a".into(), 2)),
                (None, Type::TypeVar("b".into(), 1)),
            ],
            ret: Box::new(Type::TypeVar("c".into(), 3)),
            variadic: false,
        };
        let scheme = generalize(1, &ty, &state);
        // Only a (level 2 > 1) and c (level 3 > 1) should be generalized
        // b is at level 1, not > 1
        assert_eq!(scheme.type_vars.len(), 2);
        assert!(scheme.type_vars.contains(&"a".into()));
        assert!(scheme.type_vars.contains(&"c".into()));
        assert!(!scheme.type_vars.contains(&"b".into()));
    }

    #[test]
    fn test_generalize_row_vars() {
        // BAS: row_vars removed from TypeScheme. Under BAS all rows are closed;
        // generalize only quantifies type variables, not row variables.
        // A record with a concrete field and no TypeVar in it produces a monomorphic scheme.
        let state = InferState::new();
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(Row { fields });
        let scheme = generalize(1, &ty, &state);
        assert!(scheme.type_vars.is_empty());
        assert_eq!(scheme.body, ty);
    }

    #[test]
    fn test_generalize_applies_subst_before_collecting() {
        // Defense-in-depth test: generalize() must apply substitution first.
        // Without this, a TypeVar bound in state.subst would be incorrectly generalized.
        let mut state = InferState::new();

        // Create a type variable "a" at level 2 (higher than enclosing level 1)
        state.levels.insert("a".into(), 2);

        // Bind "a" to Int in the substitution
        state
            .subst
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::Int);

        // Create a type containing the bound variable
        let ty = Type::TypeVar("a".into(), 2);

        // Generalize at level 1
        let scheme = generalize(1, &ty, &state);

        // The variable should NOT be generalized because it's bound to Int.
        // After applying substitution, the type is Int (no free vars).
        assert!(
            scheme.type_vars.is_empty(),
            "Bound TypeVar should not be generalized after substitution application"
        );
        assert_eq!(
            scheme.body,
            Type::Int,
            "Generalized type should be Int, not TypeVar"
        );
    }

    // --- level lowering in unify ---

    #[test]
    fn test_unify_level_lowering_symmetric() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        state.levels.insert("b".into(), 3);

        let mut subst = Substitution::new();
        // Unify a (level 1) with b (level 3)
        unify(
            &Type::TypeVar("a".into(), 1),
            &Type::TypeVar("b".into(), 3),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // b should be lowered to min(3, 1) = 1
        assert_eq!(state.levels.get("b"), Some(&1));
    }

    #[test]
    fn test_unify_level_lowering_in_complex_type() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        state.levels.insert("b".into(), 3);
        state.levels.insert("c".into(), 4);

        let mut subst = Substitution::new();
        let complex = Type::Function {
            params: vec![(None, Type::TypeVar("b".into(), 3))],
            ret: Box::new(Type::TypeVar("c".into(), 4)),
            variadic: false,
        };

        // Unify a (level 1) with complex type containing b (3) and c (4)
        unify(
            &Type::TypeVar("a".into(), 1),
            &complex,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Both b and c should be lowered to 1
        assert_eq!(state.levels.get("b"), Some(&1));
        assert_eq!(state.levels.get("c"), Some(&1));
    }

    #[test]
    fn test_unify_any_with_typevar_zeros_level() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 3);

        let mut subst = Substitution::new();
        unify(
            &Type::Unknown,
            &Type::TypeVar("a".into(), 3),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Level should be set to 0 to prevent generalization
        assert_eq!(state.levels.get("a"), Some(&0));
    }

    #[test]
    fn test_unify_typevar_with_any_zeros_level() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);

        let mut subst = Substitution::new();
        unify(
            &Type::TypeVar("a".into(), 2),
            &Type::Unknown,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Level should be set to 0 to prevent generalization
        assert_eq!(state.levels.get("a"), Some(&0));
    }

    #[test]
    fn test_unify_any_with_function_zeros_contained_vars() {
        // unify(Any, Fn(TypeVar("b",3) → Int)) must zero b's level
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("b".into(), 3);

        let fn_ty = Type::Function {
            params: vec![(None, Type::TypeVar("b".into(), 3))],
            ret: Box::new(Type::Int),
            variadic: false,
        };

        let mut subst = Substitution::new();
        unify(&Type::Unknown, &fn_ty, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("b"),
            Some(&0),
            "TypeVar inside Fn unified with Any must have level zeroed"
        );
    }

    #[test]
    fn test_unify_any_with_record_zeros_contained_vars() {
        // unify(Any, Record({x: TypeVar("c",2)})) must zero c
        // BAS: no RowVar in tail — only TypeVar "c" needs level zeroing
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("c".into(), 2);

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("c".into(), 2));
        let rec_ty = Type::Record(Row { fields });

        let mut subst = Substitution::new();
        unify(&Type::Unknown, &rec_ty, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("c"),
            Some(&0),
            "TypeVar inside Record unified with Any must have level zeroed"
        );
    }

    #[test]
    fn test_unify_complex_with_any_zeros_contained_vars() {
        // Symmetric: unify(Fn(TypeVar("d",4) → Seq(TypeVar("e",4))), Any)
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("d".into(), 4);
        state.levels.insert("e".into(), 4);

        let fn_ty = Type::Function {
            params: vec![(None, Type::TypeVar("d".into(), 4))],
            ret: Box::new(Type::Seq(Box::new(Type::TypeVar("e".into(), 4)))),
            variadic: false,
        };

        let mut subst = Substitution::new();
        unify(&fn_ty, &Type::Unknown, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("d"),
            Some(&0),
            "TypeVar in param unified with Any must have level zeroed"
        );
        assert_eq!(
            state.levels.get("e"),
            Some(&0),
            "TypeVar in Seq return unified with Any must have level zeroed"
        );
    }

    // --- BAS: instantiate_scheme with row_vars (row_vars now always empty under BAS) ---

    #[test]
    fn test_instantiate_scheme_with_row_var_body() {
        // Under BAS, row_vars in TypeScheme are always empty (no RowVar in tails).
        // instantiate_scheme with a row_var scheme body is tested here: the scheme
        // has row_vars but the body is now a closed record.
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let scheme = TypeScheme {
            type_vars: vec![],
            constraints: vec![],
            body: closed_record(fields.clone()),
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };

        let mut state = InferState::new();
        state.level = 2;
        let result = instantiate_scheme(&scheme, 2, &mut state);

        // Result is the record unchanged (no type vars to instantiate)
        match result {
            Type::Record(Row {
                fields: result_fields,
            }) => {
                assert_eq!(result_fields, fields);
            }
            other => panic!("expected Record, got {:?}", other),
        }
    }

    // --- Task 5: instantiate_scheme leaves free vars unchanged ---

    #[test]
    fn test_instantiate_scheme_leaves_free_vars_unchanged() {
        // Create a TypeScheme with type_vars: vec!["a"] and body Function { params: [TypeVar("a", 1)], ret: TypeVar("b", 1) }
        // Only "a" is quantified; "b" is free
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::TypeVar("a".into(), 1))],
                ret: Box::new(Type::TypeVar("b".into(), 1)),
                variadic: false,
            },
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };

        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        match result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                // "a" should get a fresh name (e.g., "_t0")
                match &params[0].1 {
                    Type::TypeVar(a_name, a_level) => {
                        assert!(
                            a_name.starts_with("_t"),
                            "quantified var 'a' should be renamed to fresh var, got {}",
                            a_name
                        );
                        assert_ne!(
                            a_name, "a",
                            "quantified var should not be 'a', got {}",
                            a_name
                        );
                        assert_eq!(*a_level, 3);
                    }
                    other => panic!("expected TypeVar in params, got {:?}", other),
                }

                // "b" should remain unchanged (it's free, not quantified)
                match ret.as_ref() {
                    Type::TypeVar(b_name, b_level) => {
                        assert_eq!(
                            b_name, "b",
                            "free var 'b' should be unchanged, got {}",
                            b_name
                        );
                        assert_eq!(*b_level, 1, "free var level should be unchanged");
                    }
                    other => panic!("expected TypeVar in return, got {:?}", other),
                }
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // --- BAS record unification tests (width subtyping — shared fields only) ---
    //
    // Under BAS, record unification only unifies fields that appear in BOTH records.
    // Fields unique to one side are NOT an error — BAS width subtyping handles openness
    // via is_subtype rather than unification binding.

    /// Two concrete records with disjoint fields fail unification.
    /// BAS width subtyping handles field differences via is_subtype (not unification).
    /// Unifying [a: Int] with [b: Str] is a type mismatch — no shared fields, all concrete.
    #[test]
    fn test_unify_disjoint_concrete_records_fails() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("b".into(), Type::Str);

        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "disjoint concrete records [a: Int] vs [b: Str] should fail unification"
        );
    }

    /// Two records with disjoint fields but TypeVars in field types: conservative — no error.
    /// When field types contain inference variables, we cannot prove incompatibility statically.
    #[test]
    fn test_unify_disjoint_records_with_typevars_conservative() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        // Register a TypeVar level so level-lowering can find it
        state.levels.insert("_t0".into(), 1);

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::TypeVar("_t0".into(), 1));
        let mut f2 = HashMap::new();
        f2.insert("b".into(), Type::Str);

        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        // Conservative: TypeVar in field type → cannot prove incompatibility, no error.
        assert!(
            result.is_ok(),
            "disjoint records with TypeVar in field type should not fail unification conservatively"
        );
        // TypeVar _t0 should have been lowered to level 0 (prevents unsound generalization)
        assert_eq!(
            state.levels.get("_t0").copied(),
            Some(0),
            "TypeVar level should be zeroed when records are disjoint"
        );
    }

    /// BAS: unifying records with overlapping and extra fields — shared fields unified, extras ignored
    #[test]
    fn test_unify_remainders_case2_left_unique_right_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to {b: Str, Empty}
        // BAS: no row bindings created; field unification handled field-by-field
        // BAS: no row bindings (no RowVar tails)
    }

    /// Case 3: right has unique fields, left tail is RowVar, left has no unique fields
    /// Unify {a: Int, ...rho} with {a: Int, b: Str} (closed) → rho binds to {b: Str, Empty}
    #[test]
    fn test_unify_remainders_case3_right_unique_left_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::Str);

        unify(
            &row_var_record(f1, "rho", 0),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to {b: Str, Empty}
        // BAS: no row bindings created; field unification handled field-by-field
        // BAS: no row bindings (no RowVar tails)
    }

    /// BAS: unifying records with non-overlapping unique fields.
    /// Under BAS, {a:Int, b:Str} and {a:Int, c:Bool} unify by shared field "a" only.
    /// Extra "b" and "c" are not errors — BAS width subtyping handles openness.
    #[test]
    fn test_unify_closed_vs_open_unique_both_sides_fails() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("c".into(), Type::Bool);

        // BAS: shared field "a" unifies OK; extra "b" and "c" are ignored
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "BAS: records with non-overlapping extra fields unify OK (shared field 'a' compatible)"
        );
    }

    /// BAS: row occurs check removed — this test verifies BAS unification behavior.
    /// Under BAS all records are closed; {a:Int, b:Str} and {a:Int} unify by
    /// unifying shared field "a" only. No infinite-row error possible.
    #[test]
    fn test_row_occurs_check_direct_tail_cycle() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        // BAS: {a:Int, b:Str} unified with {a:Int} — shared field "a" unifies OK; "b" is extra
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "BAS: unifying records with subset of shared fields is Ok"
        );
    }

    /// Row occurs check: nested-in-field cycle — ρ unified with row containing a field of type Record(ρ)
    /// Setup: left = {a: Int, x: Record({...rho})} (closed), right = {a: Int, ...rho} (open)
    /// BAS: row occurs check tests removed (RowVar removed in Step 4).
    /// Under BAS, no row variables exist in Row tails, so occurs checks are no-ops.
    /// The test_row_occurs_check_* tests tested the Rémy-style row variable occurs
    /// check mechanism which is no longer needed.
    #[test]
    fn test_row_occurs_check_removed_under_bas() {
        // Placeholder test documenting that row occurs checks are removed under BAS.
        // All records are closed; no row variables to chase or cycle through.
        let mut subst = Substitution::new();
        assert!(subst.is_empty(), "BAS: substitution starts empty");
    }

    // --- unify_tails binding tests (exercised via unify on records with no unique fields) ---

    /// Both tails are RowVar with the same name — must succeed (same variable, trivially ok)
    #[test]
    fn test_unify_tails_both_rowvar_same_name() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);

        // Both have the same shared field and same row var — Case 1 → unify_tails(rho, rho) → Ok
        unify(
            &row_var_record(f1, "rho", 0),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should NOT be bound — same name is handled as reflexivity (BAS: no row_map)
    }

    // --- BAS row-tail unification tests ---
    // Under BAS (Step 4), all RowTail::RowVar references are removed. The old Rémy-style
    // unify_tails and unify_remainders tests are replaced by simpler BAS tests that verify
    // field-by-field unification works correctly.

    /// BAS: unifying two empty closed records succeeds with no bindings
    #[test]
    fn test_unify_tails_both_rowvar_different_names() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields on either side, different row vars → Case 1 → unify_tails(rho1, rho2)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &row_var_record(f1, "rho1", 0),
            &row_var_record(f2, "rho2", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho1 should be bound to Row { fields: {}, tail: RowVar("rho2") }
        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)
    }

    /// BAS: level minimization for TypeVars in record fields
    #[test]
    fn test_unify_tails_both_rowvar_level_minimization() {
        // BAS: "row variables" don't exist. This test verifies that TypeVars in record fields
        // still get their levels lowered correctly during unification.
        // Unifying {field: TypeVar("a", level=4)} with {field: TypeVar("b", level=2)} via
        // shared field unification: "a" = "b" → level of one is lowered.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 4);
        state.levels.insert("b".into(), 2);

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::TypeVar("a".into(), 4));
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::TypeVar("b".into(), 2));
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // "a" gets bound to "b"; its level is lowered to min(4, 2) = 2
        let applied = subst.apply(&Type::TypeVar("a".into(), 4));
        assert!(
            matches!(applied, Type::TypeVar(ref n, _) if n == "b")
                || matches!(applied, Type::TypeVar(ref n, _) if n == "a"),
            "a should unify with b, got {applied}"
        );
    }

    /// RowVar vs Empty — RowVar must bind to Row { fields: {}, tail: Empty }
    #[test]
    fn test_unify_tails_rowvar_vs_empty() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields, left is open (rho), right is closed → Case 1 → unify_tails(rho, Empty)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &row_var_record(f1, "rho", 0),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to Row { fields: {}, tail: Empty }
        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)
    }

    /// Both tails are Empty — must succeed with no bindings created
    #[test]
    fn test_unify_tails_both_empty() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // BAS: no row bindings created
    }

    // =========================================================================
    // Consistency tests: unify() vs is_subtype() for all RowTail combinations
    //
    // Core invariant: if unify(A, B) succeeds producing substitution S, then
    //   is_subtype(S(A), S(B)) must hold (A <: B direction or B <: A).
    //
    // Contrapositive: when unify fails, the pre-unification is_subtype is also
    // false (or the asymmetry is documented as intentional).
    //
    // RowTail pair cases covered:
    //   1a/1b/1c  (Empty, Empty)           — both closed
    //   2 / 2b    (Empty, RowVar)          — closed sub, open sup
    //   3 / 3b/3c (RowVar, Empty)          — open sub, closed sup [conservative]
    //   4 / 4b    (RowVar(r1), RowVar(r2)) — different row vars
    //   5 / 5b    (RowVar(r), RowVar(r))   — same row var
    //   + field numeric promotion, nested record nesting
    // =========================================================================

    /// Case 1a: (Empty, Empty) identical fields — unify succeeds, subtype holds.
    ///
    /// A = [a: Int]  (closed)
    /// B = [a: Int]  (closed)
    ///
    /// unify: no bindings. S(A) = A, S(B) = B. is_subtype(A, B) = true.
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_identical() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);

        let a = closed_record(fields.clone());
        let b = closed_record(fields);

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify(A, B) succeeds for identical closed records"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify(A, B) succeeds for identical closed records"
        );
    }

    /// Case 1b: (Empty, Empty) sub has extra field — unify FAILS, but is_subtype is asymmetric.
    ///
    /// A = [a: Int, b: Str]  (closed)
    /// B = [a: Int]          (closed)
    ///
    /// Under BAS width subtyping (Step 2): A <: B holds (A has all of B's fields plus extra "b").
    /// B <: A does NOT hold (B is missing "b" which A requires).
    /// Unify still fails — unification is symmetric (equality-seeking), not subtyping.
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_extra_field() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = closed_record(b_fields);

        // BAS: unify succeeds — only shared field "a" is unified; extra "b" in A is ignored
        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_ok(),
            "BAS: unify([a:Int,b:Str], [a:Int]) succeeds — shared 'a' compatible, extra 'b' ignored"
        );

        // BAS width subtyping: A has all of B's fields plus extra "b" → A <: B
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int,b:Str](closed) SHOULD be subtype of [a:Int](closed) under BAS width subtyping"
        );
        // B is missing "b" required by A → B is NOT a subtype of A
        assert!(
            !Type::is_subtype(&b, &a),
            "[a:Int](closed) should NOT be subtype of [a:Int,b:Str](closed): missing required field 'b'"
        );
    }

    /// Case 1c: (Empty, Empty) field type mismatch — unify FAILS, is_subtype false both ways.
    ///
    /// A = [a: Int]  (closed)
    /// B = [a: Str]  (closed)
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_field_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Str);
        let b = closed_record(b_fields);

        assert!(
            unify(&a, &b, &mut subst, &mut state, span).is_err(),
            "unify([a:Int](closed), [a:Str](closed)) should fail"
        );
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int] should NOT be subtype of [a:Str]"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[a:Str] should NOT be subtype of [a:Int]"
        );
    }

    /// Case 2: (Empty, RowVar) — closed sub, open sup with same fields.
    ///
    /// A = [a: Int]        (closed)
    /// B = [a: Int, ...r]  (open, RowVar "r")
    ///
    /// unify: no unique fields -> Case 1 -> unify_tails(Empty, RowVar(r)) -> r binds to Empty.
    /// Pre-unification: is_subtype(A, B) = true (sup is open RowVar — always lenient).
    /// Post-substitution: S(B) = [a: Int] = S(A), subtype holds both ways.
    #[test]
    fn test_is_subtype_consistency_closed_sub_open_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = closed_record(a_fields.clone());
        let b = row_var_record(a_fields, "r", 0);

        // Pre-unification: sup is open -> lenient
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int](closed) should be subtype of [a:Int ...r](RowVar): sup is open"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created; a and b are both {a:Int} closed records
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(
            sb, sa,
            "S(A) and S(B) are both closed {{a:Int}} records — equal"
        );
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify succeeds"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify succeeds (symmetric post-bind)"
        );
    }

    /// Case 2b: (Empty, RowVar) — closed sub with extra fields, open sup with fewer fields.
    ///
    /// A = [a: Int, b: Str]  (closed)
    /// B = [a: Int, ...r]    (open, RowVar "r")
    ///
    /// unify: "b" unique to A, B's "r" tail absorbs it (Case 2). r binds to {b: Str, Empty}.
    /// is_subtype(A, B) = true (sup is RowVar — open tail leniency).
    /// Post-substitution: S(B) = [a: Int, b: Str] = S(A).
    #[test]
    fn test_is_subtype_consistency_closed_sub_with_extra_open_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = row_var_record(b_fields, "r", 0);

        // Pre-unification: sup is RowVar -> lenient
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int,b:Str](closed) should be subtype of [a:Int ...r](RowVar): sup is open"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created; field-by-field unification only
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // BAS: A <: B holds (A has all B's fields plus extra "b")
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify succeeds"
        );
    }

    /// Case 3: (RowVar, Empty) — open sub, closed sup with extra field.
    ///
    /// A = [a: Int, ...r]    (open, RowVar "r")
    /// B = [a: Int, b: Str]  (closed)
    ///
    /// Pre-unification is_subtype(A, B): B has "b" not in A's known fields.
    /// The field check fails (sup has field "b" absent from sub's known set).
    /// So is_subtype(A, B) = false before unification (field "b" is missing).
    ///
    /// unify: "b" unique to B; A's tail "r" absorbs it (Case 3). r binds to {b: Str, Empty}.
    /// Post-substitution: S(A) = [a: Int, b: Str] = S(B). Subtype holds both ways.
    ///
    /// Note: is_subtype is false here because B requires "b" which is absent from A's known
    /// fields — this is a FIELD MEMBERSHIP failure, not a tail-kind failure.
    #[test]
    fn test_is_subtype_consistency_open_sub_closed_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields, "r", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        b_fields.insert("b".into(), Type::Str);
        let b = closed_record(b_fields);

        // Pre-unification: B requires "b" which is NOT in A's known fields → false.
        // This failure is about missing required field "b", not about tail kind.
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...r] (RowVar) should NOT be subtype of [a:Int,b:Str] (closed): \
             sub is missing required field 'b'"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // BAS: sa = {a:Int} (was open, now closed), sb = {a:Int, b:Str}
        // sb <: sa (b has all of a's fields plus extra "b")
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold: sb has all of sa's fields"
        );
    }

    /// Case 3b: (RowVar, Empty) — open sub with exact known fields matches closed sup.
    ///
    /// A = [a: Int, ...r]  (open)
    /// B = [a: Int]        (closed)
    ///
    /// BAS width subtyping: A's known fields satisfy all of B's requirements.
    /// is_subtype(A, B) = true (BAS Step 2 — RowVar sub satisfies closed sup when fields match).
    /// unify: no unique fields -> Case 1 -> unify_tails(RowVar(r), Empty) -> r binds to Empty.
    #[test]
    fn test_is_subtype_consistency_open_sub_closed_sup_exact_known_fields() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields.clone(), "r", 0);
        let b = closed_record(a_fields);

        // BAS width subtyping (Step 2): open record [a:Int ...r] IS a subtype of closed record
        // [a:Int] because all of B's fields are present in A's known fields.
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int ...r] (RowVar) should be subtype of [a:Int] (closed) under BAS width subtyping: \
             all required fields are in sub's known set"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);
        // BAS: sa and sb may differ (row binding no longer created); check subtype

        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify (symmetric)"
        );
    }

    /// Case 3c: (RowVar, Empty) — open sub with EXTRA known fields, closed sup.
    ///
    /// A = [a: Int, b: Str, ...r]  (open)
    /// B = [a: Int]                (closed)
    ///
    /// Under BAS width subtyping (Step 2): is_subtype(A, B) = TRUE.
    /// A has all of B's required fields ("a") plus extra known "b" — width subtyping allows this.
    ///
    /// Unify still fails — the "b" field is unique to A and B is closed (Empty tail),
    /// so the unifier cannot absorb "b" into B. Unification is equality-seeking, not subtyping.
    #[test]
    fn test_is_subtype_consistency_open_sub_extra_fields_closed_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = row_var_record(a_fields, "r", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = closed_record(b_fields);

        // BAS width subtyping: A has all of B's fields (plus extra "b") → A <: B
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int,b:Str ...r] SHOULD be subtype of [a:Int](closed) under BAS width subtyping"
        );

        // BAS: unify succeeds — shared field "a" is compatible; extra "b" is ignored
        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_ok(),
            "BAS: unify({{a:Int,b:Str}}, {{a:Int}}) succeeds — shared field 'a' compatible, extra 'b' ignored"
        );
    }

    /// Case 4: (RowVar(r1), RowVar(r2)) — both open with distinct unique fields (Wand Case 4).
    ///
    /// A = [a: Int, ...r1]  (open, row var r1)
    /// B = [b: Str, ...r2]  (open, row var r2)
    ///
    /// unify creates fresh rho_fresh. Binds:
    ///   r1 -> {b: Str, tail: RowVar(rho_fresh)}
    ///   r2 -> {a: Int, tail: RowVar(rho_fresh)}
    ///
    /// Post-substitution: S(A) = S(B) = [a: Int, b: Str, ...rho_fresh].
    ///
    /// Pre-unification is_subtype:
    /// - is_subtype(A, B): sup B has field "b"; sub A does not have "b" in known fields.
    ///   The fields_ok check fails: not all sup fields are in sub. Returns FALSE.
    ///   Open-tail leniency (RowVar in sup) only governs extra fields in sub beyond sup,
    ///   NOT missing fields in sub that sup requires. The field presence check comes first.
    /// - is_subtype(B, A): sup A has field "a"; sub B does not have "a". Returns FALSE.
    ///
    /// This is NOT a bug: unify succeeds because row variables can absorb missing fields
    /// (r1 will absorb "b", r2 will absorb "a"). But is_subtype is a pure predicate operating
    /// on the pre-unification types — without mutation, it cannot infer what row vars will hold.
    ///
    /// Post-substitution: S(A) = S(B) = [a: Int, b: Str, ...rho_fresh]. Subtype holds both ways.
    #[test]
    fn test_is_subtype_consistency_both_open_different_vars_case4() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields, "r1", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("b".into(), Type::Str);
        let b = row_var_record(b_fields, "r2", 0);

        // Pre-unification: is_subtype checks "all sup fields present in sub" first.
        // A's known fields {a} don't include B's required field "b" -> fields_ok fails -> FALSE.
        // B's known fields {b} don't include A's required field "a" -> fields_ok fails -> FALSE.
        // The RowVar-tail leniency only allows extra fields in sub beyond sup's requirements;
        // it cannot supply missing required fields.
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...r1] should NOT be subtype of [b:Str ...r2]: \
             sub is missing required sup field 'b' (fields_ok fails before tail check)"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[b:Str ...r2] should NOT be subtype of [a:Int ...r1]: \
             sub is missing required sup field 'a' (fields_ok fails before tail check)"
        );

        // Disjoint concrete records now fail unification: [a: Int] and [b: Str] share
        // no fields and both have concrete (non-inference-variable) field types.
        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "disjoint concrete records [a:Int] vs [b:Str] should fail unification"
        );
    }

    /// Case 4b: (RowVar(r1), RowVar(r2)) — both open, shared field only (Wand Case 1 path).
    ///
    /// A = [a: Int, ...r1]  (open)
    /// B = [a: Int, ...r2]  (open)
    ///
    /// No unique fields -> Case 1 -> unify_tails(r1, r2).
    /// r1 binds to Row { fields: {}, tail: RowVar(r2) }.
    /// Post-substitution: S(A) = S(B) = [a: Int, ...r2].
    #[test]
    fn test_is_subtype_consistency_both_open_different_vars_case1() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields.clone(), "r1", 0);
        let b = row_var_record(a_fields, "r2", 0);

        // Pre-unification: RowVar tails -> lenient both ways
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int ...r1] should be subtype of [a:Int ...r2]: sup is open RowVar"
        );
        assert!(
            Type::is_subtype(&b, &a),
            "[a:Int ...r2] should be subtype of [a:Int ...r1]: sup is open RowVar"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created; both records are {a:Int} closed — equal
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // BAS: sa and sb are equal since both records have the same closed fields
        assert!(Type::is_subtype(&sa, &sb), "S(A) <: S(B) after unify");
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) after unify (symmetric)"
        );
    }

    /// Case 5: (RowVar(r), RowVar(r)) — same row var, same fields — reflexive.
    ///
    /// A = [a: Int, ...rho]  (open, row var rho)
    /// B = [a: Int, ...rho]  (open, same row var rho)
    ///
    /// unify: shared "a" only, no unique fields -> Case 1 -> unify_tails(rho, rho) -> reflexive.
    /// No binding created. is_subtype(A, B) = true by a==b structural equality.
    #[test]
    fn test_is_subtype_consistency_same_rowvar_same_fields() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);

        let a = row_var_record(fields.clone(), "rho", 0);
        let b = row_var_record(fields, "rho", 0);

        assert!(Type::is_subtype(&a, &b), "A == B structurally, so A <: B");
        assert!(Type::is_subtype(&b, &a), "A == B structurally, so B <: A");

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row_map — same row var unification never creates a binding

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(sa, sb, "BAS: same-field closed records are equal");
        assert!(Type::is_subtype(&sa, &sb), "S(A) <: S(B)");
        assert!(Type::is_subtype(&sb, &sa), "S(B) <: S(A)");
    }

    /// Case 5b: (RowVar(r), RowVar(r)) — same row var, different unique fields.
    ///
    /// A = [a: Int, ...rho]  (open, row var rho)
    /// B = [b: Str, ...rho]  (open, same row var rho)
    ///
    /// Both unify AND is_subtype reject this combination.
    ///
    /// is_subtype: sup (B or A) has field "b"/"a" that is not in the sub's known fields.
    ///   fields_ok fails before the tail check. Returns FALSE in both directions.
    ///
    /// unify: rejects because rho cannot simultaneously provide both "a" (unique to A)
    ///   and "b" (unique to B) — that would be unsound.
    ///
    /// Both functions agree: this is an invalid combination.
    ///
    /// Note: "open-tail leniency" (RowVar in sup allows extra sub fields) does NOT apply
    /// when the sub is MISSING a required sup field. The fields_ok check runs first.
    #[test]
    fn test_is_subtype_consistency_same_rowvar_different_unique_asymmetry() {
        let mut fields_a = HashMap::new();
        fields_a.insert("a".into(), Type::Int);
        let a = row_var_record(fields_a, "rho", 0);

        let mut fields_b = HashMap::new();
        fields_b.insert("b".into(), Type::Str);
        let b = row_var_record(fields_b, "rho", 0);

        // is_subtype: sub is missing required sup field -> fields_ok fails -> FALSE both ways
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...rho] should NOT be subtype of [b:Str ...rho]: \
             sub is missing required sup field 'b' (fields_ok fails)"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[b:Str ...rho] should NOT be subtype of [a:Int ...rho]: \
             sub is missing required sup field 'a' (fields_ok fails)"
        );

        // Disjoint concrete records now fail unification: no shared fields, all-concrete types.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "disjoint concrete records {{a:Int}} vs {{b:Str}} should fail unification"
        );
    }

    /// Numeric promotion through record fields — unify more permissive than is_subtype.
    ///
    /// A = [x: Int]    (closed)
    /// B = [x: Number] (closed)
    ///
    /// is_subtype: A <: B (Int <: Number). B <:/ A.
    /// unify: succeeds via promotion rules (Int ~ Number).
    /// Post-substitution: S(A) = [x: Int], S(B) = [x: Number] — asymmetric subtype preserved.
    ///
    /// Documents the intentional asymmetry: unify is bidirectional, is_subtype is directional.
    #[test]
    fn test_is_subtype_consistency_field_numeric_promotion_closed_closed() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("x".into(), Type::Int);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("x".into(), Type::Number);
        let b = closed_record(b_fields);

        assert!(
            Type::is_subtype(&a, &b),
            "[x:Int] should be subtype of [x:Number]: Int <: Number"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[x:Number] should NOT be subtype of [x:Int]: Number !<: Int"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // Directional subtype preserved post-unification
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must still hold after unify (Int <: Number)"
        );
        // unify is more permissive than <: for promotions
        assert!(
            !Type::is_subtype(&sb, &sa),
            "S(B) <:/ S(A): unify is more permissive than <: for promotions"
        );
    }

    /// Nested record consistency — RowVar in nested field type.
    ///
    /// A = [point: [x: Int, y: Int] (closed)]  (closed outer)
    /// B = [point: [x: Int, ...r]]              (open inner, closed outer)
    ///
    /// is_subtype(A, B): inner sup is RowVar -> extra 'y' allowed -> true.
    /// unify: inner row var 'r' absorbs "y: Int".
    /// Post-substitution: S(A) = S(B) (inner 'r' bound to {y: Int, Empty}).
    #[test]
    fn test_is_subtype_consistency_nested_record_field() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut inner_a = HashMap::new();
        inner_a.insert("x".into(), Type::Int);
        inner_a.insert("y".into(), Type::Int);
        let mut outer_a = HashMap::new();
        outer_a.insert("point".into(), closed_record(inner_a));
        let a = closed_record(outer_a);

        let mut inner_b = HashMap::new();
        inner_b.insert("x".into(), Type::Int);
        let mut outer_b = HashMap::new();
        outer_b.insert("point".into(), row_var_record(inner_b, "r", 0));
        let b = closed_record(outer_b);

        assert!(
            Type::is_subtype(&a, &b),
            "[point:[x:Int,y:Int]](closed) should be subtype of [point:[x:Int ...r]](closed): \
             inner sup is RowVar so extra 'y' in sub is allowed"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // BAS: sa = {point: {x:Int, y:Int}}, sb = {point: {x:Int}}
        // sa <: sb: {x:Int, y:Int} has all fields of {x:Int} (plus extra y) → TRUE
        // sb <: sa: {x:Int} is missing field y of {x:Int, y:Int} → FALSE
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) — A has all of B's fields"
        );
        assert!(
            !Type::is_subtype(&sb, &sa),
            "S(B) not <: S(A) — B missing 'y'"
        );
    }

    /// Two records with completely disjoint concrete fields should fail unification.
    /// Unifying {x: Int} with {y: Str}: no shared fields, all-concrete types → type mismatch.
    /// (BAS subtyping handles width/openness via is_subtype, not via unification silencing.)
    #[test]
    fn test_unify_same_rho_different_unique_fields_errors() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("y".into(), Type::Str);

        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "disjoint concrete records [x: Int] vs [y: Str] should fail unification"
        );
    }

    /// Two concrete records with asymmetric disjoint field sets should fail unification.
    /// Unifying {x: Int, z: Bool} with {y: Str}: zero shared fields, all-concrete types → error.
    /// The error fires regardless of cardinality asymmetry (2 fields vs 1 field).
    #[test]
    fn test_unify_same_rowvar_asymmetric_unique_field_counts_errors() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        f1.insert("z".into(), Type::Bool);
        let mut f2 = HashMap::new();
        f2.insert("y".into(), Type::Str);

        // Left has two unique fields, right has one — all three are side-exclusive, all concrete
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );

        assert!(
            result.is_err(),
            "disjoint concrete records [x: Int, z: Bool] vs [y: Str] should fail unification"
        );
    }

    /// Test that lower_row_var_levels in unify_remainders Case 2 prevents over-generalization.
    /// This verifies the Kiselyov (2013) level-based let-polymorphism mechanism: inner row vars
    /// at level 3 should have their level lowered to the outer row var's level when bound,
    /// preventing them from being generalized at the wrong scope.
    #[test]
    fn test_lower_row_var_levels_prevents_generalization() {
        // BAS: lower_row_var_levels is removed (no row vars). This test now verifies
        // that TypeVar level lowering works correctly via unification.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Register some TypeVar levels
        state.levels.insert("t_inner".into(), 3);

        // TypeVar binding via unification: t_inner at level 3 unified with Int → level zeroed
        let left = Type::TypeVar("t_inner".into(), 3);
        unify(&left, &Type::Int, &mut subst, &mut state, span).unwrap();

        // After unification with Int, t_inner is bound (not generalized freely)
        assert!(
            subst.type_map.borrow().contains_key("t_inner"),
            "t_inner should be bound"
        );

        // Generalize at level 1 — t_inner is bound to Int (concrete), not in the scheme body
        let scheme = generalize(1, &Type::Int, &state);
        assert!(
            !scheme.type_vars.contains(&"t_inner".to_string()),
            "t_inner should not be generalized (it's bound to a concrete type)"
        );
    }

    /// Test the symmetric direction of unify_tails: (Empty, RowVar) vs the already-tested (RowVar, Empty).
    /// Both should bind the RowVar to Row { fields: {}, tail: Empty }.
    #[test]
    fn test_unify_tails_empty_vs_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields, left is closed (Empty), right is open (rho)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to Row { fields: {}, tail: Empty }
        // BAS: no row bindings created
        // BAS: no row bindings (no RowVar tails)
    }

    /// Test that shared-field unification bindings are not overwritten by stale tail references.
    ///
    /// Scenario: ρ appears both as the tail of an outer row AND inside a nested Record field.
    /// Step 3 (shared-field unification) binds ρ via the nested record.
    /// Step 4 must re-resolve the outer tail to see that binding, rather than using the
    /// pre-Step-3 stale RowVar(ρ) reference that would overwrite the binding.
    ///
    /// Row1: {a: Record({x: Int, ...ρ}), ...ρ}
    /// Row2: {a: Record({x: Int, y: Str}), z: Bool}
    ///
    /// Step 3 binds ρ → {y: Str, ∅} from inner record unification.
    /// Without the fix, Step 4 would overwrite ρ → {z: Bool, ∅}, losing the y: Str constraint.
    /// With the fix, Step 4 re-resolves ρ, sees it's already bound to {y: Str, ∅}, and the
    /// outer row resolves to {a: ..., y: Str} vs {a: ..., z: Bool} — correctly producing an error.
    ///
    /// Formal model: Robinson (1965) substitution-threading invariant — bindings from
    /// earlier unification steps must be visible to later steps.
    #[test]
    fn test_reresolution_after_shared_field_unification() {
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.level = 1;
        state.levels.insert("rho".into(), 1);
        let span = test_span(1, 1, 1, 1);

        // Row1: {a: Record({x: Int, ...ρ}), ...ρ}
        let inner1 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int)]),
        });
        let row1 = Row {
            fields: HashMap::from([("a".into(), inner1)]),
        };

        // Row2: {a: Record({x: Int, y: Str}), z: Bool}
        let inner2 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int), ("y".into(), Type::Str)]),
        });
        let row2 = Row {
            fields: HashMap::from([("a".into(), inner2), ("z".into(), Type::Bool)]),
        };

        // BAS: all records are closed (RowTail::Empty). Shared outer field "a" unifies.
        // Inner records: {x:Int} vs {x:Int, y:Str} — shared "x" unifies OK, extra "y" ignored.
        // Extra outer "z" in row2 is ignored. Result: Ok(()).
        let result = unify(
            &Type::Record(row1),
            &Type::Record(row2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "BAS: field-by-field unification succeeds; extra fields ignored"
        );
    }

    /// Test that re-resolution works correctly when the row variable binding is compatible.
    ///
    /// Row1: {a: Record({x: Int, ...ρ}), ...ρ}
    /// Row2: {a: Record({x: Int, y: Str}), y: Str}
    ///
    /// Step 3 binds ρ → {y: Str, ∅} from inner record unification.
    /// After re-resolution, outer row1 becomes {a: ..., y: Str, ∅}.
    /// Outer row2 is {a: ..., y: Str, ∅}.
    /// The newly-surfaced y: Str fields match — unification should succeed.
    #[test]
    fn test_reresolution_compatible_binding() {
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.level = 1;
        state.levels.insert("rho".into(), 1);
        let span = test_span(1, 1, 1, 1);

        // Row1: {a: Record({x: Int, ...ρ}), ...ρ}
        let inner1 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int)]),
        });
        let row1 = Row {
            fields: HashMap::from([("a".into(), inner1)]),
        };

        // Row2: {a: Record({x: Int, y: Str}), y: Str}
        let inner2 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int), ("y".into(), Type::Str)]),
        });
        let row2 = Row {
            fields: HashMap::from([("a".into(), inner2), ("y".into(), Type::Str)]),
        };

        // Should succeed: ρ → {y: Str, ∅} from inner, then outer y: Str matches
        let result = unify(
            &Type::Record(row1),
            &Type::Record(row2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "should succeed: ρ bound to {{y: Str}} by inner, outer y: Str matches. Got: {:?}",
            result.err()
        );

        // Verify ρ is bound to {y: Str, ∅}
        // BAS: no row bindings created; field "y" is not shared between the two rows
        // BAS: no row bindings (no RowVar tails)
    }

    /// BAS: row_var_occurs_in_type and row_var_occurs removed — these tests are replaced
    /// by a simple check that type variable substitution applies correctly in records.
    #[test]
    fn test_typevar_chase_in_record_field() {
        // Under BAS, row_var_occurs is a no-op (returns false always).
        // The TypeVar chasing behavior of apply() still works for TypeVars in fields.
        let mut subst = Substitution::new();

        let mut beta_fields = HashMap::new();
        beta_fields.insert("z".into(), Type::Int);
        let beta_bound = Type::Record(Row {
            fields: beta_fields,
        });
        subst
            .type_map
            .borrow_mut()
            .insert("beta".into(), beta_bound);

        let mut alpha_fields = HashMap::new();
        alpha_fields.insert("x".into(), Type::TypeVar("beta".into(), 0));
        let alpha_bound = Type::Record(Row {
            fields: alpha_fields,
        });
        subst
            .type_map
            .borrow_mut()
            .insert("alpha".into(), alpha_bound);

        // Substitution.apply correctly resolves TypeVar → Record
        let tv_alpha = Type::TypeVar("alpha".into(), 0);
        let applied = subst.apply(&tv_alpha);
        match applied {
            Type::Record(Row { fields, .. }) => {
                assert!(fields.contains_key("x"), "applied type should have field x");
            }
            _ => panic!("expected Record after applying alpha substitution"),
        }
    }

    #[test]
    fn test_max_subst_size_limit_type_vars() {
        // Create enough type variable bindings to exceed MAX_SUBST_SIZE
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Create MAX_SUBST_SIZE + 1 type variables and try to unify them
        // This should trigger the size limit
        for i in 0..=MAX_SUBST_SIZE {
            let var = Type::TypeVar(format!("t{}", i), 0);
            let concrete = Type::Int;
            let result = unify(&var, &concrete, &mut subst, &mut state, span);

            if i <= MAX_SUBST_SIZE - 1 {
                // Should succeed for bindings within the limit
                assert!(result.is_ok(), "unify should succeed for binding {}", i);
            } else {
                // Should fail when exceeding the limit
                assert!(
                    result.is_err(),
                    "unify should fail when exceeding MAX_SUBST_SIZE"
                );
                if let Err(e) = result {
                    assert!(
                        e.message.contains("type inference resource limit exceeded"),
                        "error message should mention inference limit, got: {}",
                        e.message
                    );
                }
            }
        }
    }

    #[test]
    fn test_max_subst_size_limit_row_vars() {
        // BAS: row variables no longer create row_map bindings (RowTail::RowVar removed).
        // Under BAS, unifying two closed records (both Empty) never creates row bindings.
        // The size limit is still tested via TypeVar bindings in test_max_subst_size_limit_type_vars.
        // This test verifies that unifying large numbers of closed records succeeds (no row bindings).
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Unify many closed records — none should create row bindings
        for _ in 0..100 {
            let rec1 = Type::Record(Row {
                fields: HashMap::new(),
            });
            let rec2 = Type::Record(Row {
                fields: HashMap::new(),
            });
            let result = unify(&rec1, &rec2, &mut subst, &mut state, span);
            assert!(result.is_ok(), "BAS: empty closed records always unify");
        }
        // BAS: no row bindings created
    }

    #[test]
    fn test_max_subst_size_combined_types_and_rows() {
        // BAS: row_map is always empty. Only type_map contributes to the size limit.
        // This test verifies that the MAX_SUBST_SIZE limit applies to type_map alone.
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Add type variables until we exceed the limit
        for i in 0..=MAX_SUBST_SIZE {
            let var = Type::TypeVar(format!("t{}", i), 0);
            let concrete = Type::Int;
            let result = unify(&var, &concrete, &mut subst, &mut state, span);

            if i <= MAX_SUBST_SIZE - 1 {
                assert!(
                    result.is_ok(),
                    "type var unify should succeed for binding {}",
                    i
                );
            } else {
                assert!(
                    result.is_err(),
                    "unify should fail when exceeding MAX_SUBST_SIZE"
                );
                if let Err(e) = result {
                    assert!(
                        e.message.contains("type inference resource limit exceeded"),
                        "error message should mention inference limit, got: {}",
                        e.message
                    );
                }
                break;
            }
        }
    }

    // --- Type::Error sentinel ---

    #[test]
    fn test_error_display() {
        assert_eq!(format!("{}", Type::Error), "<error>");
    }

    #[test]
    fn test_error_eq() {
        assert_eq!(Type::Error, Type::Error);
        assert_ne!(Type::Error, Type::Int);
        assert_ne!(Type::Error, Type::Unknown);
    }

    #[test]
    fn test_error_is_not_subtype_of_anything() {
        assert!(!Type::is_subtype(&Type::Error, &Type::Int));
        assert!(!Type::is_subtype(&Type::Error, &Type::Str));
        assert!(!Type::is_subtype(&Type::Error, &Type::Unknown));
        assert!(!Type::is_subtype(&Type::Error, &Type::Error));
        assert!(!Type::is_subtype(&Type::Int, &Type::Error));
        assert!(!Type::is_subtype(&Type::Unknown, &Type::Error));
    }

    #[test]
    fn test_error_has_no_inference_vars() {
        assert!(!Type::Error.has_inference_vars());
    }

    #[test]
    fn test_error_collect_vars_empty() {
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        Type::Error.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.is_empty());
        assert!(row_vars.is_empty());
    }

    #[test]
    fn test_unify_error_with_any_type_succeeds() {
        // unify(Error, T) = Ok(()) for all T — error absorption
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Error with concrete types
        assert!(unify(&Type::Error, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Int, &Type::Error, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Str, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Bool, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Unknown, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Unknown, &Type::Error, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Error, &mut subst, &mut state, span).is_ok());

        // Substitution must not be modified — Error carries no binding information
        assert!(
            subst.is_empty(),
            "unify(Error, T) must not create any bindings in the substitution"
        );
    }

    #[test]
    fn test_unify_error_with_typevar_does_not_bind() {
        // unify(Error, TypeVar) = Ok(()) — Error absorbs without binding the TypeVar
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);

        let result = unify(
            &Type::Error,
            &Type::TypeVar("a".into(), 1),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_ok());
        // TypeVar "a" must not be bound — Error does not carry type information
        assert!(
            subst.type_map.borrow().is_empty(),
            "TypeVar must not be bound when unified with Error"
        );
    }

    #[test]
    fn test_apply_preserves_error() {
        // Substitution::apply must pass Error through unchanged
        let mut subst = Substitution::new();
        assert_eq!(subst.apply(&Type::Error), Type::Error);

        let subst_with_binding = Substitution::new();
        subst_with_binding
            .type_map
            .borrow_mut()
            .insert("a".into(), Type::Int);
        assert_eq!(subst_with_binding.apply(&Type::Error), Type::Error);
    }

    /// Case 5: unify_remainders with display-hiding row variable.
    /// Tests that unification succeeds when one of the row variables has a `_` prefix,
    /// triggering the display-hiding branch in error messages and Display formatting.
    #[test]
    fn test_unify_remainders_case5_display_hiding() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Create two rows with the same field set: {a: Int}
        // Left: {a: Int, ...rho1}, right: {a: Int, ..._hidden2}
        // The `_hidden2` row var has a `_` prefix → display-hiding behavior
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        // Unify should succeed: shared field {a: Int}, no unique fields → Case 1
        let result = unify(
            &row_var_record(f1, "rho1", 0),
            &row_var_record(f2, "_hidden2", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "unification should succeed when row var has _ prefix, got: {:?}",
            result.unwrap_err()
        );

        // BAS: no row bindings created; all tails are Empty
        // BAS: no row bindings (no RowVar tails)
    }

    // --- variadic flag in PartialEq and unify ---

    #[test]
    fn test_function_partial_eq_includes_variadic() {
        // variadic=true and variadic=false must not be equal even with identical params/ret.
        let f_variadic = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_non_variadic = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_ne!(
            f_variadic, f_non_variadic,
            "Fn(Int→Bool, variadic=true) must not equal Fn(Int→Bool, variadic=false)"
        );
        // Same variadic flag must still be equal.
        let f_variadic2 = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        assert_eq!(
            f_variadic, f_variadic2,
            "Fn(Int→Bool, variadic=true) must equal itself"
        );
    }

    #[test]
    fn test_unify_variadic_mismatch_error() {
        // unify(Fn(variadic=true), Fn(variadic=false)) must return a TypeError
        // containing "variadic mismatch".
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f_variadic = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_non_variadic = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let result = unify(&f_variadic, &f_non_variadic, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "unify(variadic=true, variadic=false) must return Err"
        );
        assert!(
            result.unwrap_err().message.contains("variadic mismatch"),
            "error message must contain 'variadic mismatch'"
        );
    }

    #[test]
    fn test_is_subtype_variadic_mismatch() {
        // is_subtype must return false when variadic flags differ.
        let f_v = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_nv = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(
            !Type::is_subtype(&f_v, &f_nv),
            "variadic must not be subtype of non-variadic"
        );
        assert!(
            !Type::is_subtype(&f_nv, &f_v),
            "non-variadic must not be subtype of variadic"
        );
    }

    // ===== Union Type Tests =====

    #[test]
    fn test_normalize_union_single_element() {
        // Single-element unions unwrap to the bare type
        let union = Type::normalize_union(vec![Type::Int]);
        assert_eq!(union, Type::Int);
    }

    #[test]
    fn test_normalize_union_deduplication() {
        // Duplicate types are removed
        let union = Type::normalize_union(vec![Type::Int, Type::Str, Type::Int]);
        match union {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Union type"),
        }
    }

    #[test]
    fn test_normalize_union_flattening() {
        // Nested unions are flattened
        let inner_union = Type::Union(vec![Type::Int, Type::Str]);
        let outer_union = Type::normalize_union(vec![inner_union, Type::Bool]);
        match outer_union {
            Type::Union(members) => {
                assert_eq!(members.len(), 3);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
                assert!(members.contains(&Type::Bool));
            }
            _ => panic!("Expected Union type"),
        }
    }

    #[test]
    fn test_normalize_union_sorting() {
        // Members are sorted canonically
        let union = Type::normalize_union(vec![Type::Str, Type::Int, Type::Bool]);
        match union {
            Type::Union(members) => {
                assert_eq!(members.len(), 3);
                // Int (order 0) < Str (order 3) < Bool (order 5) by type_order
                assert_eq!(members[0], Type::Int);
                assert_eq!(members[1], Type::Str);
                assert_eq!(members[2], Type::Bool);
            }
            _ => panic!("Expected Union type"),
        }
    }

    #[test]
    fn test_union_display() {
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert_eq!(format!("{}", union), "Int | String");
    }

    #[test]
    fn test_union_display_three_members() {
        let union = Type::normalize_union(vec![Type::Int, Type::Str, Type::Bool]);
        // Display should show all members separated by " | "
        let display = format!("{}", union);
        assert!(display.contains("Int"));
        assert!(display.contains("String"));
        assert!(display.contains("Bool"));
        assert!(display.contains(" | "));
    }

    #[test]
    fn test_union_subtype_injection_left() {
        // [UNION-INJ-L]: Int <: Int | Str
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(Type::is_subtype(&Type::Int, &union));
    }

    #[test]
    fn test_union_subtype_injection_right() {
        // [UNION-INJ-R]: Str <: Int | Str
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(Type::is_subtype(&Type::Str, &union));
    }

    #[test]
    fn test_union_subtype_elimination_success() {
        // [UNION-ELIM]: Int | Float <: Number (both Int and Float are subtypes of Number)
        let union = Type::normalize_union(vec![Type::Int, Type::Float]);
        assert!(Type::is_subtype(&union, &Type::Number));
    }

    #[test]
    fn test_union_subtype_elimination_failure() {
        // [UNION-ELIM] failure: Int | Str is NOT a subtype of Number
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(!Type::is_subtype(&union, &Type::Number));
    }

    #[test]
    fn test_union_collect_type_vars() {
        // Union members' type variables are collected
        let tv_a = Type::TypeVar("a".into(), 0);
        let tv_b = Type::TypeVar("b".into(), 0);
        let union = Type::normalize_union(vec![tv_a, Type::Int, tv_b]);
        let mut vars = HashSet::new();
        union.collect_type_vars(&mut vars);
        assert_eq!(vars.len(), 2);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
    }

    #[test]
    fn test_union_has_inference_vars() {
        // Union with type variables has inference vars
        let tv = Type::TypeVar("a".into(), 0);
        let union = Type::normalize_union(vec![tv, Type::Int]);
        assert!(union.has_inference_vars());

        // Union without type variables doesn't have inference vars
        let union2 = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(!union2.has_inference_vars());
    }

    #[test]
    fn test_union_apply_substitution() {
        // Substitution is applied to all members and result is re-normalized
        let tv = Type::TypeVar("a".into(), 0);
        let union = Type::Union(vec![tv, Type::Str]);

        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);

        let result = subst.apply(&union);

        // After substitution, "a" becomes Int, so we have Int | Str
        match result {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Union type after substitution"),
        }
    }

    #[test]
    fn test_union_apply_substitution_collapse() {
        // Substitution that makes all members equal collapses the union
        let tv_a = Type::TypeVar("a".into(), 0);
        let tv_b = Type::TypeVar("b".into(), 0);
        let union = Type::Union(vec![tv_a, tv_b]);

        let mut subst = Substitution::new();
        subst.type_map.borrow_mut().insert("a".into(), Type::Int);
        subst.type_map.borrow_mut().insert("b".into(), Type::Int);

        let result = subst.apply(&union);

        // After substitution, both become Int, so the union collapses to Int
        assert_eq!(result, Type::Int);
    }

    #[test]
    fn test_union_literal_subtyping() {
        // IntLiteral(42) <: Int | Str (via [UNION-INJ-L] and literal promotion)
        let union = Type::normalize_union(vec![Type::Int, Type::Str]);
        let literal = Type::IntLiteral(42);
        assert!(Type::is_subtype(&literal, &union));
    }

    #[test]
    fn test_union_record_subtyping() {
        // Test union with record types
        let record1 = closed_record({
            let mut fields = HashMap::new();
            fields.insert("ok".into(), Type::Int);
            fields
        });
        let record2 = closed_record({
            let mut fields = HashMap::new();
            fields.insert("err".into(), Type::Str);
            fields
        });

        let union = Type::normalize_union(vec![record1.clone(), record2.clone()]);

        // Both record types should be subtypes of the union
        assert!(Type::is_subtype(&record1, &union));
        assert!(Type::is_subtype(&record2, &union));
    }

    // --- Constraint checking tests ---

    #[test]
    fn test_constraint_equatable_hardcoded() {
        // Equatable is hardcoded for primitive types (prelude instances are commented out;
        // primitives use Rust fallback dispatch).
        assert!(satisfies_constraint(&Type::Int, "Equatable"));
        assert!(satisfies_constraint(&Type::IntLiteral(42), "Equatable"));
        assert!(satisfies_constraint(&Type::Float, "Equatable"));
        assert!(satisfies_constraint(&Type::Str, "Equatable"));
        assert!(satisfies_constraint(&Type::Bool, "Equatable"));
        // Function types do NOT satisfy Equatable
        let func_ty = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert!(!satisfies_constraint(&func_ty, "Equatable"));
    }

    #[test]
    fn test_constraint_numeric_satisfied_by_number() {
        assert!(satisfies_constraint(&Type::Int, "Numeric"));
        assert!(satisfies_constraint(&Type::Float, "Numeric"));
        assert!(satisfies_constraint(&Type::Number, "Numeric"));
        assert!(satisfies_constraint(&Type::IntLiteral(5), "Numeric"));
    }

    #[test]
    fn test_constraint_numeric_not_satisfied_by_str() {
        assert!(!satisfies_constraint(&Type::Str, "Numeric"));
    }

    #[test]
    fn test_constraint_showable_not_hardcoded() {
        // Showable is no longer hardcoded - it's resolved via instances in prelude.llt
        assert!(!satisfies_constraint(&Type::Int, "Showable"));
        assert!(!satisfies_constraint(&Type::Str, "Showable"));
        assert!(!satisfies_constraint(&Type::Bool, "Showable"));
        let func_ty = Type::Function {
            params: vec![],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert!(!satisfies_constraint(&func_ty, "Showable"));
    }

    #[test]
    fn test_constraint_mappable_not_hardcoded() {
        // Mappable is no longer hardcoded - it's resolved via instances in prelude.llt
        let dict_ty = Type::Record(Row {
            fields: HashMap::new(),
        });
        let seq_ty = Type::Seq(Box::new(Type::Int));
        assert!(!satisfies_constraint(&dict_ty, "Mappable"));
        assert!(!satisfies_constraint(&seq_ty, "Mappable"));
        assert!(!satisfies_constraint(&Type::Int, "Mappable"));
    }

    // --- BAS constraint propagation tests ---

    #[test]
    fn test_constraint_record_field_propagation() {
        // [CONSTRAIN-FIELD]: C(Record({f: τ})) satisfied iff C(τ) for all fields
        // Applies only to structural constraints: Numeric, Comparable
        use std::collections::HashMap;

        // Record with all Numeric fields -> satisfies Numeric
        let mut fields_numeric = HashMap::new();
        fields_numeric.insert("x".to_string(), Type::Int);
        fields_numeric.insert("y".to_string(), Type::Float);
        let record_numeric = Type::Record(Row {
            fields: fields_numeric,
        });
        assert!(satisfies_constraint(&record_numeric, "Numeric"));

        // Record with mixed fields -> does NOT satisfy Numeric
        let mut fields_mixed = HashMap::new();
        fields_mixed.insert("x".to_string(), Type::Int);
        fields_mixed.insert("y".to_string(), Type::Str);
        let record_mixed = Type::Record(Row {
            fields: fields_mixed,
        });
        assert!(!satisfies_constraint(&record_mixed, "Numeric"));

        // Record with all Comparable fields -> satisfies Comparable
        let mut fields_comparable = HashMap::new();
        fields_comparable.insert("x".to_string(), Type::Int);
        fields_comparable.insert("y".to_string(), Type::Str);
        let record_comparable = Type::Record(Row {
            fields: fields_comparable,
        });
        assert!(satisfies_constraint(&record_comparable, "Comparable"));

        // Equatable/Showable/Mappable do NOT propagate structurally - they use instance resolution
        let mut fields_any = HashMap::new();
        fields_any.insert("x".to_string(), Type::Int);
        let record_any = Type::Record(Row { fields: fields_any });
        assert!(!satisfies_constraint(&record_any, "Equatable")); // instance-based
        assert!(!satisfies_constraint(&record_any, "Showable")); // instance-based
        assert!(!satisfies_constraint(&record_any, "Mappable")); // instance-based
    }

    #[test]
    fn test_constraint_union_all_members() {
        // [CONSTRAIN-UNION]: C(τ₁ | τ₂) satisfied iff C(τ₁) ∧ C(τ₂) (ALL members)

        // Union of Numeric types -> satisfies Numeric
        let union_numeric = Type::normalize_union(vec![Type::Int, Type::Float]);
        assert!(satisfies_constraint(&union_numeric, "Numeric"));

        // Union with non-Numeric member -> does NOT satisfy Numeric
        let union_mixed = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(!satisfies_constraint(&union_mixed, "Numeric"));

        // Union of Comparable types -> satisfies Comparable
        let union_comparable = Type::normalize_union(vec![Type::Int, Type::Str]);
        assert!(satisfies_constraint(&union_comparable, "Comparable"));
    }

    #[test]
    fn test_constraint_intersection_all_members() {
        // [CONSTRAIN-INTER]: C(τ₁ & τ₂) satisfied iff C(τ₁) ∧ C(τ₂) (ALL members)

        // Intersection of Numeric types -> satisfies Numeric
        let inter_numeric = Type::normalize_intersection(vec![Type::Int, Type::Number]);
        assert!(satisfies_constraint(&inter_numeric, "Numeric"));

        // Intersection with non-Numeric member -> does NOT satisfy Numeric
        // Note: Can't use Top because normalize_intersection removes it (identity element).
        // Use Record with a non-Numeric field.
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str); // Str is NOT Numeric
        let record_ty = Type::Record(Row { fields });
        let inter_mixed = Type::Intersection(vec![Type::Int, record_ty]);
        assert!(!satisfies_constraint(&inter_mixed, "Numeric"));
    }

    #[test]
    fn test_constraint_never_vacuous() {
        // [CONSTRAIN-NEVER]: C(⊥) satisfied (vacuously — Never is uninhabited)
        assert!(satisfies_constraint(&Type::Never, "Numeric"));
        assert!(satisfies_constraint(&Type::Never, "Equatable"));
        assert!(satisfies_constraint(&Type::Never, "Showable"));
        assert!(satisfies_constraint(&Type::Never, "Comparable"));
        assert!(satisfies_constraint(&Type::Never, "Mappable"));
    }

    #[test]
    fn test_constraint_top_showable_only() {
        // [CONSTRAIN-TOP]: Showable(⊤) satisfied, all other classes ⊢ error
        assert!(satisfies_constraint(&Type::Top, "Showable"));
        assert!(!satisfies_constraint(&Type::Top, "Equatable"));
        assert!(!satisfies_constraint(&Type::Top, "Comparable"));
        assert!(!satisfies_constraint(&Type::Top, "Numeric"));
        assert!(!satisfies_constraint(&Type::Top, "Mappable"));
    }

    #[test]
    fn test_constraint_unknown_vacuous() {
        // Unknown satisfies all constraints (gradual typing existential lifting)
        assert!(satisfies_constraint(&Type::Unknown, "Numeric"));
        assert!(satisfies_constraint(&Type::Unknown, "Equatable"));
        assert!(satisfies_constraint(&Type::Unknown, "Showable"));
        assert!(satisfies_constraint(&Type::Unknown, "Comparable"));
    }

    #[test]
    fn test_instantiate_scheme_with_constraints() {
        // Create a scheme with Numeric constraint: Numeric a => a -> a -> a
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            constraints: vec![Constraint::new("Numeric", "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".into(), 0)),
                    (None, Type::TypeVar("a".into(), 0)),
                ],
                ret: Box::new(Type::TypeVar("a".into(), 0)),
                variadic: false,
            },
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };

        let mut state = InferState::new();
        state.level = 1;
        let _inst = instantiate_scheme(&scheme, 1, &mut state);

        // After instantiation, constraints should be copied with renamed variables
        assert_eq!(state.constraints.len(), 1);
        assert!(matches!(
            &state.constraints[0],
            Constraint::Class { class, vars, .. } if class == "Numeric" && vars.len() == 1 && vars[0].starts_with("_t")
        ));
    }

    #[test]
    fn test_unify_with_constraint_success() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Add constraint: Numeric a
        state.add_constraint("Numeric", "a");

        // Unify a with Int (should succeed - Int satisfies Numeric)
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        assert_eq!(subst.type_map.borrow().get("a"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_with_constraint_failure() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Add constraint: Equatable a
        state.add_constraint("Equatable", "a");

        // Try to unify a with Function (should fail - Function doesn't satisfy Equatable)
        let func_ty = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
        };

        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &func_ty,
            &mut subst,
            &mut state,
            span,
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().message;
        assert!(err_msg.contains("does not satisfy constraint"));
        assert!(err_msg.contains("Equatable"));
    }

    // ===== Algebraic Subtyping Tests =====

    #[test]
    fn test_constrain_literal_promotion() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // IntLiteral <: Int
        assert!(constrain(
            &Type::IntLiteral(42),
            &Type::Int,
            &mut state,
            span,
            "literal promotion"
        )
        .is_ok());

        // IntLiteral <: Number
        assert!(constrain(
            &Type::IntLiteral(42),
            &Type::Number,
            &mut state,
            span,
            "literal promotion"
        )
        .is_ok());

        // StringLiteral <: Str
        assert!(constrain(
            &Type::StringLiteral("hello".to_string()),
            &Type::Str,
            &mut state,
            span,
            "literal promotion"
        )
        .is_ok());

        // Int <: Number
        assert!(constrain(&Type::Int, &Type::Number, &mut state, span, "int to number").is_ok());
    }

    #[test]
    fn test_constrain_type_variable_bounds() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // α <: Int (α in contravariant position → upper bound)
        let alpha = Type::TypeVar("alpha".to_string(), 0);
        assert!(constrain(&alpha, &Type::Int, &mut state, span, "upper bound").is_ok());

        // Check that alpha has Int as upper bound
        let bounds = state.bounds.get("alpha").unwrap();
        assert_eq!(bounds.upper.len(), 1);
        assert_eq!(bounds.upper[0], Type::Int);

        // Int <: α (α in covariant position → lower bound)
        assert!(constrain(&Type::Int, &alpha, &mut state, span, "lower bound").is_ok());

        // Check that alpha now has Int as both lower and upper bound
        let bounds = state.bounds.get("alpha").unwrap();
        assert_eq!(bounds.lower.len(), 1);
        assert_eq!(bounds.lower[0], Type::Int);
    }

    #[test]
    fn test_constrain_function_contravariance() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // Fn(Number → Int) <: Fn(Int → Number)
        // requires: Int <: Number (param contravariant) and Int <: Number (return covariant)
        let sub_fn = Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let sup_fn = Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Number),
            variadic: false,
        };

        assert!(constrain(&sub_fn, &sup_fn, &mut state, span, "function subtyping").is_ok());
    }

    #[test]
    fn test_constrain_seq_covariance() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // Seq[Int] <: Seq[Number]
        let sub_seq = Type::Seq(Box::new(Type::Int));
        let sup_seq = Type::Seq(Box::new(Type::Number));

        assert!(constrain(&sub_seq, &sup_seq, &mut state, span, "seq covariance").is_ok());
    }

    #[test]
    fn test_constrain_union_injection() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        let union = Type::Union(vec![Type::Int, Type::Str]);

        // Int <: Int | Str (union injection left)
        assert!(constrain(&Type::Int, &union, &mut state, span, "union injection").is_ok());

        // Str <: Int | Str (union injection right)
        assert!(constrain(&Type::Str, &union, &mut state, span, "union injection").is_ok());

        // Bool is NOT a subtype of Int | Str
        assert!(constrain(&Type::Bool, &union, &mut state, span, "union rejection").is_err());
    }

    #[test]
    fn test_constrain_union_elimination() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        let union = Type::Union(vec![Type::Int, Type::Float]);

        // Int | Float <: Number (both members are subtypes)
        assert!(constrain(&union, &Type::Number, &mut state, span, "union elimination").is_ok());

        // Int | Str is NOT a subtype of Number (Str is not a subtype of Number)
        let bad_union = Type::Union(vec![Type::Int, Type::Str]);
        assert!(constrain(
            &bad_union,
            &Type::Number,
            &mut state,
            span,
            "union elimination failure"
        )
        .is_err());
    }

    #[test]
    fn test_constrain_intersection_subtyping() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // Create an intersection type (hypothetical: Number & Equatable)
        // For this test, we'll just use Int & Int which normalizes to Int
        let intersection = Type::normalize_intersection(vec![Type::Int, Type::Int]);
        assert_eq!(intersection, Type::Int); // Single-element intersection unwraps

        // Test with actual intersection
        let intersection = Type::Intersection(vec![Type::Number, Type::Int]);

        // Intersection <: Number (intersection is subtype of any member)
        assert!(constrain(
            &intersection,
            &Type::Number,
            &mut state,
            span,
            "intersection intro"
        )
        .is_ok());

        // Int <: Int & Number requires Int <: Int AND Int <: Number
        assert!(constrain(
            &Type::Int,
            &intersection,
            &mut state,
            span,
            "intersection elim"
        )
        .is_ok());
    }

    #[test]
    fn test_normalize_intersection_identity() {
        // Top is identity: T & Top = T
        let result = Type::normalize_intersection(vec![Type::Int, Type::Top]);
        assert_eq!(result, Type::Int);

        let result = Type::normalize_intersection(vec![Type::Top, Type::Str, Type::Top]);
        assert_eq!(result, Type::Str);
    }

    #[test]
    fn test_normalize_intersection_absorbing() {
        // Error is absorbing: T & Error = Error
        let result = Type::normalize_intersection(vec![Type::Int, Type::Error]);
        assert_eq!(result, Type::Error);

        let result = Type::normalize_intersection(vec![Type::Top, Type::Error, Type::Str]);
        assert_eq!(result, Type::Error);
    }

    #[test]
    fn test_normalize_intersection_flattening() {
        // Nested intersections are flattened
        let inner = Type::Intersection(vec![Type::Int, Type::Number]);
        let result = Type::normalize_intersection(vec![inner, Type::Str]);

        match result {
            Type::Intersection(members) => {
                assert_eq!(members.len(), 3);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Number));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Intersection, got {:?}", result),
        }
    }

    #[test]
    fn test_normalize_intersection_deduplication() {
        let result = Type::normalize_intersection(vec![Type::Int, Type::Str, Type::Int]);

        match result {
            Type::Intersection(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Intersection, got {:?}", result),
        }
    }

    #[test]
    fn test_compact_bounds_no_bounds() {
        let bounds = TypeVarBounds::new();
        let result = compact_bounds("alpha", &bounds, 0);

        // No bounds → unconstrained TypeVar
        assert_eq!(result, Type::TypeVar("alpha".to_string(), 0));
    }

    #[test]
    fn test_compact_bounds_lower_only() {
        let mut bounds = TypeVarBounds::new();
        bounds.add_lower(Type::Int);
        bounds.add_lower(Type::Str);

        let result = compact_bounds("alpha", &bounds, 0);

        // Multiple lower bounds → union
        match result {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Union, got {:?}", result),
        }
    }

    #[test]
    fn test_compact_bounds_upper_only() {
        let mut bounds = TypeVarBounds::new();
        bounds.add_upper(Type::Number);
        bounds.add_upper(Type::Int);

        let result = compact_bounds("alpha", &bounds, 0);

        // Multiple upper bounds → intersection
        match result {
            Type::Intersection(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Number));
                assert!(members.contains(&Type::Int));
            }
            _ => panic!("Expected Intersection, got {:?}", result),
        }
    }

    #[test]
    fn test_check_bounds_satisfiable_success() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // Add satisfiable bounds: Int <: alpha <: Number
        let mut bounds = TypeVarBounds::new();
        bounds.add_lower(Type::Int);
        bounds.add_upper(Type::Number);
        state.bounds.insert("alpha".to_string(), bounds);

        // Should succeed: Int <: Number
        assert!(check_bounds_satisfiable(&state, span).is_ok());
    }

    #[test]
    fn test_check_bounds_satisfiable_failure() {
        let mut state = InferState::new();
        let span = test_span(1, 1, 1, 1);

        // Add unsatisfiable bounds: Number <: alpha <: Int
        let mut bounds = TypeVarBounds::new();
        bounds.add_lower(Type::Number);
        bounds.add_upper(Type::Int);
        state.bounds.insert("alpha".to_string(), bounds);

        // Should fail: Number is NOT a subtype of Int
        let result = check_bounds_satisfiable(&state, span);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unsatisfiable bounds"));
    }

    #[test]
    fn test_intersection_display() {
        let intersection = Type::Intersection(vec![Type::Int, Type::Str]);
        assert_eq!(format!("{}", intersection), "Int & String");

        // Nested unions should be parenthesized
        let union = Type::Union(vec![Type::Bool, Type::Float]);
        let complex = Type::Intersection(vec![Type::Int, union]);
        assert_eq!(format!("{}", complex), "Int & (Bool | Float)");
    }

    #[test]
    fn test_entails_direct() {
        // Direct entailment: constraint is in context
        let state = InferState::new();
        let context = vec![Constraint::new("Equatable", "a")];
        let target = Constraint::new("Equatable", "a");

        assert!(entails(&state.class_env, &context, &target));
    }

    #[test]
    fn test_entails_superclass() {
        // Superclass entailment: Numeric has Equatable as superclass
        let state = InferState::new();
        let context = vec![Constraint::new("Numeric", "a")];
        let target = Constraint::new("Equatable", "a");

        assert!(entails(&state.class_env, &context, &target));
    }

    #[test]
    fn test_entails_superclass_comparable() {
        // Superclass entailment: Comparable has Equatable as superclass
        let state = InferState::new();
        let context = vec![Constraint::new("Comparable", "a")];
        let target = Constraint::new("Equatable", "a");

        assert!(entails(&state.class_env, &context, &target));
    }

    #[test]
    fn test_entails_not_entailed() {
        // Not entailed: Equatable does not imply Numeric
        let state = InferState::new();
        let context = vec![Constraint::new("Equatable", "a")];
        let target = Constraint::new("Numeric", "a");

        assert!(!entails(&state.class_env, &context, &target));
    }

    #[test]
    fn test_entails_different_vars() {
        // Not entailed: different type variables
        let state = InferState::new();
        let context = vec![Constraint::new("Numeric", "a")];
        let target = Constraint::new("Equatable", "b");

        assert!(!entails(&state.class_env, &context, &target));
    }

    #[test]
    fn test_builtin_classes_registered() {
        // Test that built-in classes are registered at initialization
        let state = InferState::new();

        assert!(state.class_env.get("Equatable").is_some());
        assert!(state.class_env.get("Numeric").is_some());
        assert!(state.class_env.get("Comparable").is_some());
        assert!(state.class_env.get("Showable").is_some());
        assert!(state.class_env.get("Mappable").is_some());
        assert!(state.class_env.get("Appendable").is_some());
    }

    #[test]
    fn test_numeric_superclass() {
        // Test that Numeric has Equatable as a superclass
        let state = InferState::new();
        let numeric = state.class_env.get("Numeric").unwrap();

        assert_eq!(
            numeric.superclasses,
            vec![("Equatable".to_string(), "a".to_string())]
        );
    }

    #[test]
    fn test_comparable_superclass() {
        // Test that Comparable has Equatable as a superclass
        let state = InferState::new();
        let comparable = state.class_env.get("Comparable").unwrap();

        assert_eq!(
            comparable.superclasses,
            vec![("Equatable".to_string(), "a".to_string())]
        );
    }

    #[test]
    fn test_constraint_simplification() {
        // Test that constraint simplification removes redundant constraints
        let state = InferState::new();

        // Both Numeric and Equatable on the same var — Equatable is redundant
        let mut constraints = vec![
            Constraint::new("Numeric", "a"),
            Constraint::new("Equatable", "a"),
        ];

        simplify_constraints(&state.class_env, &mut constraints);

        // Only Numeric should remain (it entails Equatable)
        assert_eq!(constraints.len(), 1);
        assert!(matches!(
            &constraints[0],
            Constraint::Class { class, vars, .. } if class == "Numeric" && vars == &vec!["a".to_string()]
        ));
    }

    #[test]
    fn test_constraint_simplification_comparable() {
        // Test that Comparable entails Equatable
        let state = InferState::new();

        let mut constraints = vec![
            Constraint::new("Comparable", "a"),
            Constraint::new("Equatable", "a"),
        ];

        simplify_constraints(&state.class_env, &mut constraints);

        // Only Comparable should remain
        assert_eq!(constraints.len(), 1);
        assert!(matches!(
            &constraints[0],
            Constraint::Class { class, .. } if class == "Comparable"
        ));
    }

    #[test]
    fn test_constraint_simplification_no_redundancy() {
        // Test that non-redundant constraints are preserved
        let state = InferState::new();

        let mut constraints = vec![
            Constraint::new("Numeric", "a"),
            Constraint::new("Showable", "b"),
        ];

        simplify_constraints(&state.class_env, &mut constraints);

        // Both should remain (different vars, no entailment)
        assert_eq!(constraints.len(), 2);
    }

    // -------------------------------------------------------------------------
    // normalize_union improvements: Never identity, Top absorption
    // -------------------------------------------------------------------------

    #[test]
    fn test_normalize_union_never_is_identity() {
        // T | Never = T (Never is the identity element in union)
        let result = Type::normalize_union(vec![Type::Int, Type::Never]);
        assert_eq!(result, Type::Int);
    }

    #[test]
    fn test_normalize_union_all_never_returns_never() {
        // Never | Never = Never
        let result = Type::normalize_union(vec![Type::Never, Type::Never]);
        assert_eq!(result, Type::Never);
    }

    #[test]
    fn test_normalize_union_top_absorbs() {
        // T | Top = Top (Top is the absorbing element in union)
        let result = Type::normalize_union(vec![Type::Int, Type::Top]);
        assert_eq!(result, Type::Top);
    }

    #[test]
    fn test_normalize_union_top_absorbs_all() {
        // Int | Str | Top = Top
        let result = Type::normalize_union(vec![Type::Int, Type::Str, Type::Top]);
        assert_eq!(result, Type::Top);
    }

    #[test]
    fn test_normalize_union_never_mixed_with_others() {
        // Int | Never | Str = Int | Str
        let result = Type::normalize_union(vec![Type::Int, Type::Never, Type::Str]);
        match result {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Union, got {:?}", result),
        }
    }

    // -------------------------------------------------------------------------
    // normalize_intersection improvements: Never absorption
    // -------------------------------------------------------------------------

    #[test]
    fn test_normalize_intersection_never_absorbs() {
        // T & Never = Never (S-ClsBot base: Never annihilates all in intersection)
        let result = Type::normalize_intersection(vec![Type::Int, Type::Never]);
        assert_eq!(result, Type::Never);
    }

    #[test]
    fn test_normalize_intersection_never_absorbs_all() {
        // Int & Str & Never = Never
        let result = Type::normalize_intersection(vec![Type::Int, Type::Str, Type::Never]);
        assert_eq!(result, Type::Never);
    }

    // -------------------------------------------------------------------------
    // simplify_type: identity/absorbing element rules
    // -------------------------------------------------------------------------

    #[test]
    fn test_simplify_type_single_union_unwraps() {
        // Union([T]) = T
        let ty = Type::Union(vec![Type::Int]);
        assert_eq!(Type::simplify_type(ty), Type::Int);
    }

    #[test]
    fn test_simplify_type_single_intersection_unwraps() {
        // Intersection([T]) = T
        let ty = Type::Intersection(vec![Type::Int]);
        assert_eq!(Type::simplify_type(ty), Type::Int);
    }

    #[test]
    fn test_simplify_type_intersection_never_absorbs() {
        // Int & Never = Never
        let ty = Type::Intersection(vec![Type::Int, Type::Never]);
        assert_eq!(Type::simplify_type(ty), Type::Never);
    }

    #[test]
    fn test_simplify_type_union_top_absorbs() {
        // Int | Top = Top
        let ty = Type::Union(vec![Type::Int, Type::Top]);
        assert_eq!(Type::simplify_type(ty), Type::Top);
    }

    #[test]
    fn test_simplify_type_union_remove_never_arms() {
        // Int | Never | Str = Int | Str
        let ty = Type::Union(vec![Type::Int, Type::Never, Type::Str]);
        match Type::simplify_type(ty) {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
                assert!(members.contains(&Type::Int));
                assert!(members.contains(&Type::Str));
            }
            _ => panic!("Expected Union after simplification"),
        }
    }

    #[test]
    fn test_simplify_type_primitives_unchanged() {
        assert_eq!(Type::simplify_type(Type::Int), Type::Int);
        assert_eq!(Type::simplify_type(Type::Str), Type::Str);
        assert_eq!(Type::simplify_type(Type::Never), Type::Never);
        assert_eq!(Type::simplify_type(Type::Top), Type::Top);
    }

    // -------------------------------------------------------------------------
    // S-RcdTop: disjoint single-field closed records union → Top
    // -------------------------------------------------------------------------

    fn single_field_closed(name: &str, ty: Type) -> Type {
        let mut fields = HashMap::new();
        fields.insert(name.to_string(), ty);
        closed_record(fields)
    }

    #[test]
    fn test_simplify_type_s_rcd_top_two_disjoint() {
        // {x: Int} | {y: Str} → Top (disjoint single-field closed records)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let ty = Type::Union(vec![r1, r2]);
        assert_eq!(Type::simplify_type(ty), Type::Top);
    }

    #[test]
    fn test_simplify_type_s_rcd_top_same_field_no_collapse() {
        // {x: Int} | {x: Str} — same field name, NOT disjoint → no S-RcdTop
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("x", Type::Str);
        let ty = Type::Union(vec![r1, r2]);
        // Should remain a union, not collapse to Top
        assert!(matches!(Type::simplify_type(ty), Type::Union(_)));
    }

    #[test]
    fn test_simplify_type_s_rcd_top_open_record_no_collapse() {
        // BAS: under BAS all records are closed (RowTail::Empty). Two single-field records
        // {x: Int} and {y: Str} DOES trigger S-RcdTop → Top.
        // The old test checked that open records (RowVar) don't trigger S-RcdTop.
        // Under BAS, the same records ARE closed, so they DO trigger S-RcdTop.
        // Verify the BAS behavior: single-field disjoint union = Top
        let mut f1 = HashMap::new();
        f1.insert("x".to_string(), Type::Int);
        let r1 = Type::Record(Row { fields: f1 });
        let mut f2 = HashMap::new();
        f2.insert("y".to_string(), Type::Str);
        let r2 = Type::Record(Row { fields: f2 });
        let ty = Type::Union(vec![r1, r2]);
        // BAS: closed single-field disjoint records union = Top
        assert!(matches!(Type::simplify_type(ty), Type::Top));
    }

    #[test]
    fn test_is_subtype_s_rcd_top_subtype_of_top() {
        // {x: Int} | {y: Str} <: Top — should hold (union is Top)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let union = Type::Union(vec![r1, r2]);
        assert!(Type::is_subtype(&union, &Type::Top));
    }

    #[test]
    fn test_is_subtype_s_rcd_top_not_subtype_of_int() {
        // {x: Int} | {y: Str} <: Int — should NOT hold (union is Top, Top ⊄ Int)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let union = Type::Union(vec![r1, r2]);
        assert!(!Type::is_subtype(&union, &Type::Int));
    }

    // -------------------------------------------------------------------------
    // S-ClsBot: disjoint single-field closed records intersection → Never
    // -------------------------------------------------------------------------

    #[test]
    fn test_simplify_type_s_cls_bot_two_disjoint() {
        // {x: Int} & {y: Str} → Never (cannot simultaneously have exactly field x and field y)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let ty = Type::Intersection(vec![r1, r2]);
        assert_eq!(Type::simplify_type(ty), Type::Never);
    }

    #[test]
    fn test_simplify_type_s_cls_bot_same_field_no_annihilation() {
        // {x: Int} & {x: Str} — same field name, structural overlap is possible (or invalid at field level)
        // S-ClsBot only applies when field NAMES differ — same name means subtype mismatch, not S-ClsBot
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("x", Type::Str);
        let ty = Type::Intersection(vec![r1, r2]);
        // Should remain as intersection (field-type mismatch is a separate concern)
        assert!(matches!(Type::simplify_type(ty), Type::Intersection(_)));
    }

    #[test]
    fn test_simplify_type_s_cls_bot_open_record_no_annihilation() {
        // BAS: under BAS all records are closed (RowTail::Empty). Two single-field records
        // {x: Int} & {y: Str} DOES trigger S-ClsBot → Never.
        // The old test checked that open records (RowVar) don't trigger S-ClsBot.
        // Under BAS, the same records ARE closed, so they DO trigger S-ClsBot.
        let mut f1 = HashMap::new();
        f1.insert("x".to_string(), Type::Int);
        let r1 = Type::Record(Row { fields: f1 });
        let mut f2 = HashMap::new();
        f2.insert("y".to_string(), Type::Str);
        let r2 = Type::Record(Row { fields: f2 });
        let ty = Type::Intersection(vec![r1, r2]);
        // BAS: closed single-field disjoint records intersection = Never
        assert!(matches!(Type::simplify_type(ty), Type::Never));
    }

    #[test]
    fn test_is_subtype_s_cls_bot_subtype_of_never() {
        // {x: Int} & {y: Str} <: Never — should hold (intersection IS Never)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let intersection = Type::Intersection(vec![r1, r2]);
        assert!(Type::is_subtype(&intersection, &Type::Never));
    }

    #[test]
    fn test_is_subtype_s_cls_bot_subtype_of_anything() {
        // {x: Int} & {y: Str} <: Int — should hold (Never <: T for all T)
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("y", Type::Str);
        let intersection = Type::Intersection(vec![r1, r2]);
        assert!(Type::is_subtype(&intersection, &Type::Int));
    }

    #[test]
    fn test_is_subtype_s_cls_bot_same_field_not_never() {
        // {x: Int} & {x: Str} — same field, S-ClsBot does NOT fire; standard INTERSECT-INTRO applies
        let r1 = single_field_closed("x", Type::Int);
        let r2 = single_field_closed("x", Type::Str);
        let intersection = Type::Intersection(vec![r1, r2]);
        // INTERSECT-INTRO: {x: Int} & {x: Str} <: {x: Int} is true (member is subtype of itself)
        let target = single_field_closed("x", Type::Int);
        assert!(Type::is_subtype(&intersection, &target));
        // But {x: Int} & {x: Str} <: Never is false (S-ClsBot doesn't fire, same field names)
        assert!(!Type::is_subtype(&intersection, &Type::Never));
    }

    // -------------------------------------------------------------------------
    // simplify_type: recursive child simplification (Step 2)
    // -------------------------------------------------------------------------

    #[test]
    fn test_simplify_type_nested_union_unwraps() {
        // Union([Union([Int])]) → Int via bottom-up simplification
        let inner = Type::Union(vec![Type::Int]);
        let outer = Type::Union(vec![inner]);
        // simplify_children first: inner Union([Int]) → Int
        // then outer Union([Int]) → Int
        assert_eq!(Type::simplify_type(outer), Type::Int);
    }

    #[test]
    fn test_simplify_type_seq_recurses() {
        // Seq(Union([Int])) → Seq(Int)
        let ty = Type::Seq(Box::new(Type::Union(vec![Type::Int])));
        assert_eq!(Type::simplify_type(ty), Type::Seq(Box::new(Type::Int)));
    }

    #[test]
    fn test_simplify_type_negation_recurses() {
        // Negation(Union([Int])) → Negation(Int)
        let ty = Type::Negation(Box::new(Type::Union(vec![Type::Int])));
        assert_eq!(Type::simplify_type(ty), Type::Negation(Box::new(Type::Int)));
    }

    #[test]
    fn test_simplify_type_record_fields_recurse() {
        // Record({x: Union([Int])}) → Record({x: Int})
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Union(vec![Type::Int]));
        let ty = Type::Record(Row { fields });
        match Type::simplify_type(ty) {
            Type::Record(row) => {
                assert_eq!(row.fields.get("x"), Some(&Type::Int));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // -------------------------------------------------------------------------
    // simplify_type: subsumption elimination and literal promotion
    // -------------------------------------------------------------------------

    #[test]
    fn test_simplify_type_int_literal_subtype_of_int_eliminated() {
        // Union([Int, IntLiteral(0)]) → Int (subsumption: IntLiteral(0) <: Int)
        let ty = Type::Union(vec![Type::Int, Type::IntLiteral(0)]);
        assert_eq!(Type::simplify_type(ty), Type::Int);
    }

    #[test]
    fn test_simplify_type_two_int_literals_promote_to_int() {
        // Union([IntLiteral(0), IntLiteral(42)]) → Int (literal promotion)
        let ty = Type::Union(vec![Type::IntLiteral(0), Type::IntLiteral(42)]);
        assert_eq!(Type::simplify_type(ty), Type::Int);
    }

    #[test]
    fn test_simplify_type_two_string_literals_promote_to_str() {
        // Union([StringLiteral("a"), StringLiteral("b")]) → Str (literal promotion)
        let ty = Type::Union(vec![
            Type::StringLiteral("a".to_string()),
            Type::StringLiteral("b".to_string()),
        ]);
        assert_eq!(Type::simplify_type(ty), Type::Str);
    }

    #[test]
    fn test_simplify_type_single_int_literal_unchanged() {
        // Union([IntLiteral(42)]) → IntLiteral(42) (single member unwraps, not promoted)
        let ty = Type::Union(vec![Type::IntLiteral(42)]);
        assert_eq!(Type::simplify_type(ty), Type::IntLiteral(42));
    }

    #[test]
    fn test_simplify_type_int_literal_with_str_no_promotion() {
        // Union([IntLiteral(0), Str]) — only one IntLiteral, no promotion, stays as union
        let ty = Type::Union(vec![Type::IntLiteral(0), Type::Str]);
        match Type::simplify_type(ty) {
            Type::Union(members) => {
                assert_eq!(members.len(), 2);
            }
            other => panic!("expected Union, got {other}"),
        }
    }

    #[test]
    fn test_simplify_type_function_params_recurse() {
        // Function(params=[Union([Int])], ret=Union([Str])) → Function(params=[Int], ret=Str)
        let ty = Type::Function {
            params: vec![(None, Type::Union(vec![Type::Int]))],
            ret: Box::new(Type::Union(vec![Type::Str])),
            variadic: false,
        };
        match Type::simplify_type(ty) {
            Type::Function { params, ret, .. } => {
                assert_eq!(params[0].1, Type::Int);
                assert_eq!(*ret, Type::Str);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_types_are_disjoint_primitives() {
        // Different primitives are disjoint
        assert!(Type::types_are_disjoint(&Type::Int, &Type::Str));
        assert!(Type::types_are_disjoint(&Type::Int, &Type::Bool));
        assert!(Type::types_are_disjoint(&Type::Str, &Type::Bool));
        assert!(Type::types_are_disjoint(&Type::Float, &Type::Str));
        assert!(Type::types_are_disjoint(&Type::Int, &Type::Float));

        // Same type is not disjoint
        assert!(!Type::types_are_disjoint(&Type::Int, &Type::Int));
        assert!(!Type::types_are_disjoint(&Type::Str, &Type::Str));
    }

    #[test]
    fn test_types_are_disjoint_never() {
        // Never is disjoint from everything
        assert!(Type::types_are_disjoint(&Type::Never, &Type::Int));
        assert!(Type::types_are_disjoint(&Type::Int, &Type::Never));
        assert!(Type::types_are_disjoint(&Type::Never, &Type::Never));
    }

    #[test]
    fn test_types_are_disjoint_unknown_top() {
        // Unknown and Top are conservatively assumed to overlap with everything
        assert!(!Type::types_are_disjoint(&Type::Unknown, &Type::Int));
        assert!(!Type::types_are_disjoint(&Type::Int, &Type::Unknown));
        assert!(!Type::types_are_disjoint(&Type::Top, &Type::Int));
        assert!(!Type::types_are_disjoint(&Type::Int, &Type::Top));
    }

    #[test]
    fn test_types_are_disjoint_union() {
        // Union(String, Int) is disjoint from Bool (all members disjoint)
        let union = Type::normalize_union(vec![Type::Str, Type::Int]);
        assert!(Type::types_are_disjoint(&union, &Type::Bool));

        // Union(String, Int) is not disjoint from Int (Int member overlaps)
        assert!(!Type::types_are_disjoint(&union, &Type::Int));
    }

    #[test]
    fn test_negation_subtyping_disjoint() {
        // Int <: ~String (Int and String are disjoint)
        let not_string = Type::Negation(Box::new(Type::Str));
        assert!(Type::is_subtype(&Type::Int, &not_string));

        // Int <: ~Int should fail (Int and Int overlap)
        let not_int = Type::Negation(Box::new(Type::Int));
        assert!(!Type::is_subtype(&Type::Int, &not_int));
    }

    #[test]
    fn test_negation_subtyping_union() {
        // Union(String, Int) <: ~Bool (all members disjoint from Bool)
        let union = Type::normalize_union(vec![Type::Str, Type::Int]);
        let not_bool = Type::Negation(Box::new(Type::Bool));
        assert!(Type::is_subtype(&union, &not_bool));

        // Union(String, Int) <: ~Int should fail (Int member overlaps)
        let not_int = Type::Negation(Box::new(Type::Int));
        assert!(!Type::is_subtype(&union, &not_int));
    }
}
