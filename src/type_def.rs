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

use crate::ast::Span;

// TypeError is defined in type_env.rs (will be moved to type_class.rs or kept in type_env.rs)
// We need to import it for check_kind_wellformed
use crate::types::TypeError;

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
/// Kinds classify types: * for proper types, Operator for type constructors (* → *)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    /// * — kind of proper types (Int, Str, [name: Str], etc.)
    Type,
    /// Operator — kind of type constructors (* → *).
    /// Used for type constructor variables like `m` in `Monad m`.
    Operator,
    /// Label — kind of type-level string labels used for record field names.
    /// Used for label TypeVars in `HasField` constraints (e.g., `key@"k"`).
    Label,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Type => write!(f, "*"),
            Kind::Operator => write!(f, "* → *"),
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
    /// File/stream handle — wraps Box<dyn BufRead>. Created by `open` or `connect`.
    /// Represents authority to read/write a specific open resource.
    /// The inner type is a Row describing capabilities (e.g., Handle[Readable Writable]).
    /// Type::Unknown as the inner type means "unknown capabilities" (gradual typing).
    Handle(Box<Type>),
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
            // TODO(handle-partialeq-limitation): Handle capability row comparison uses
            // structural equality (cap1 == cap2), which can fail when capability rows
            // contain TypeVars that should unify but have different names.
            //
            // Example: Handle(TypeVar("a", 0)) != Handle(TypeVar("b", 0)) even though
            // the two types might be unifiable in the type checker's substitution context.
            //
            // A proper fix requires bidirectional subtyping (Handle[C1] <: Handle[C2] iff
            // C1 <: C2), but PartialEq doesn't have access to the unification engine or
            // substitution context. This limitation affects:
            // - Type normalization (identical types may not be deduplicated)
            // - HashMap/HashSet usage with Type keys (false negatives in lookups)
            //
            // Does NOT affect type checking soundness: unification (src/type_unify.rs)
            // uses `unify()` which recursively unifies capability rows via substitution,
            // not PartialEq. PartialEq is only used for fast-path equality checks and
            // data structure operations, where false negatives are safe (conservative).
            (Type::Handle(cap1), Type::Handle(cap2)) => cap1 == cap2,
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
            Type::Handle(cap) => cap.hash(state),
            Type::Record(row) => {
                // Hash fields in sorted order for deterministic hashing
                let mut fields: Vec<_> = row.fields.iter().collect();
                fields.sort_by_key(|(k, _)| *k);
                fields.hash(state);
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
            Type::Seq(elem) => elem.hash(state),
            Type::Map(k, v) => {
                k.hash(state);
                v.hash(state);
            }
            Type::TypeVar(name, _) => name.hash(state), // Ignore level
            Type::Union(members) => members.hash(state),
            Type::Intersection(members) => members.hash(state),
            Type::Negation(ty) => ty.hash(state),
            Type::App(f, a) => {
                f.hash(state);
                a.hash(state);
            }
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
            }
        }
    }
}

/// Maximum recursion depth for subtype checking.
/// Prevents stack overflow on pathological recursive types (defense-in-depth).
const MAX_SUBTYPE_DEPTH: usize = 256;

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
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
        Self::is_subtype_inner(sub, sup, 0)
    }

    fn is_subtype_inner(sub: &Type, sup: &Type, depth: usize) -> bool {
        // Depth guard: prevent unbounded recursion on pathological recursive types
        if depth >= MAX_SUBTYPE_DEPTH {
            return false;
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
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::Seq(sub_elem), Type::Seq(sup_elem)) => {
                Self::is_subtype_inner(sub_elem, sup_elem, depth + 1)
            }
            // Map[K V1] <: Map[K V2] when V1 <: V2 (V covariant, K invariant via ==)
            (Type::Map(k1, v1), Type::Map(k2, v2)) => {
                k1 == k2 && Self::is_subtype_inner(v1, v2, depth + 1)
            }
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::Str) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            // [UNION-INJ-L] and [UNION-INJ-R]: any member is a subtype of the union
            (sub_ty, Type::Union(sup_members)) => sup_members
                .iter()
                .any(|member| Self::is_subtype_inner(sub_ty, member, depth + 1)),
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
                .all(|member| Self::is_subtype_inner(member, sup_ty, depth + 1)),
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
                .any(|member| Self::is_subtype_inner(member, sup_ty, depth + 1)),
            // [INTERSECT-ELIM]: type is a subtype of intersection iff it's a subtype of ALL members
            (sub_ty, Type::Intersection(sup_members)) => sup_members
                .iter()
                .all(|member| Self::is_subtype_inner(sub_ty, member, depth + 1)),
            // Negation: A <: ~B iff A and B are disjoint (for now, conservative: only reflexive negation)
            // Full BAS subtyping requires RDNF normalization — this is a placeholder
            (Type::Negation(t1), Type::Negation(t2)) => {
                Self::is_subtype_inner(t2, t1, depth + 1) // contravariant
            }
            // Negation subtyping: T <: ~A iff T and A are disjoint (no values in common).
            // Full BAS uses RDNF normalization to compute T ∩ A = Never, but we use a
            // conservative syntactic disjointness check that catches obvious cases like
            // Int <: ~String (true) and Int <: ~Int (false).
            (sub_ty, Type::Negation(a)) => Type::types_are_disjoint(sub_ty, a),
            // Handle: covariant in capability row
            // Handle[Readable Writable] <: Handle[Readable] because more capabilities satisfy fewer
            (Type::Handle(sub_cap), Type::Handle(sup_cap)) => {
                Self::is_subtype_inner(sub_cap, sup_cap, depth + 1)
            }
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
                            if !Self::is_subtype_inner(sub_ty, sup_ty, depth + 1) {
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
                        _ => return Self::is_subtype_inner(sub_r, sup_r, depth + 1),
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
                            Self::is_subtype_inner(pp_ty, sp_ty, depth + 1)
                        },
                    )
                    && Self::is_subtype_inner(sub_r, sup_r, depth + 1)
            }
            // App and Operator: structural equality for now (full BAS rules in hkt-bas).
            // App(f1, a1) <: App(f2, a2) requires f1 = f2 and a1 <: a2 (covariant).
            (Type::App(f1, a1), Type::App(f2, a2)) => {
                f1 == f2 && Self::is_subtype_inner(a1, a2, depth + 1)
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
                            if !Self::is_subtype_inner(sub_ty, sup_ty, depth + 1) {
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
            // Every type that can structurally contain Unknown gets its own arm here.
            (Type::Seq(a), Type::Seq(b)) => Self::is_consistent_subtype(a, b),
            (Type::Map(k1, v1), Type::Map(k2, v2)) => {
                Self::is_consistent_subtype(k1, k2) && Self::is_consistent_subtype(v1, v2)
            }
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
            _ => Self::is_subtype(sub, sup),
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

            // Function vs structural types (Record, Seq, Map, NominalVariant)
            (Type::Function { .. }, Type::Record(_)) => true,
            (Type::Function { .. }, Type::Seq(_)) => true,
            (Type::Function { .. }, Type::Map(_, _)) => true,
            (Type::Function { .. }, Type::NominalVariant { .. }) => true,
            (Type::Record(_), Type::Function { .. }) => true,
            (Type::Seq(_), Type::Function { .. }) => true,
            (Type::Map(_, _), Type::Function { .. }) => true,
            (Type::NominalVariant { .. }, Type::Function { .. }) => true,

            // NominalVariant vs primitives
            (Type::NominalVariant { .. }, Type::Int | Type::IntLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Float) => true,
            (Type::NominalVariant { .. }, Type::Str | Type::StringLiteral(_)) => true,
            (Type::NominalVariant { .. }, Type::Bool) => true,
            (Type::NominalVariant { .. }, Type::Bytes) => true,
            (Type::NominalVariant { .. }, Type::Seq(_)) => true,
            (Type::NominalVariant { .. }, Type::Map(_, _)) => true,
            (Type::Int | Type::IntLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Float, Type::NominalVariant { .. }) => true,
            (Type::Str | Type::StringLiteral(_), Type::NominalVariant { .. }) => true,
            (Type::Bool, Type::NominalVariant { .. }) => true,
            (Type::Bytes, Type::NominalVariant { .. }) => true,
            (Type::Seq(_), Type::NominalVariant { .. }) => true,
            (Type::Map(_, _), Type::NominalVariant { .. }) => true,

            // Union: disjoint if ALL members are disjoint from the other type
            (Type::Union(members), t) | (t, Type::Union(members)) => {
                members.iter().all(|m| Type::types_are_disjoint(m, t))
            }

            // Intersection: disjoint if ANY member is disjoint from the other type
            (Type::Intersection(members), t) | (t, Type::Intersection(members)) => {
                members.iter().any(|m| Type::types_are_disjoint(m, t))
            }

            // Two single-field records with DIFFERENT keys are disjoint (S-RcdTop).
            // {x: T} and {y: U} where x ≠ y have no values in common — no record can
            // satisfy both field requirements. This improves Negation subtyping precision
            // without requiring full RDNF normalization.
            (Type::Record(row1), Type::Record(row2))
                if row1.fields.len() == 1 && row2.fields.len() == 1 =>
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
            // Handle: consistent if capability rows are consistent
            (Type::Handle(cap1), Type::Handle(cap2)) => Type::is_consistent(cap1, cap2),
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
            Type::Handle(cap) => cap.collect_type_vars(vars),
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
            Type::TypeStageApp { fn_name: _, args } => {
                args.iter().any(|arg| arg.has_inference_vars())
            }
            Type::NominalVariant { tag: _, fields } => {
                fields.fields.values().any(|ty| ty.has_inference_vars())
            }
            Type::Handle(cap) => cap.has_inference_vars(),
            Type::Proxy => false,
            _ => false,
        }
    }

    /// Check if this type contains any TypeStageApp nodes.
    /// Used to determine if deferred equalities can be resolved.
    pub fn has_type_stage_app(&self) -> bool {
        match self {
            Type::TypeStageApp { .. } => true,
            Type::Record(row) => row.fields.values().any(|ty| ty.has_type_stage_app()),
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                params.iter().any(|(_name, p_ty)| p_ty.has_type_stage_app())
                    || ret.has_type_stage_app()
            }
            Type::Seq(elem) => elem.has_type_stage_app(),
            Type::Map(key, val) => key.has_type_stage_app() || val.has_type_stage_app(),
            Type::Union(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Intersection(members) => members.iter().any(|m| m.has_type_stage_app()),
            Type::Negation(inner) => inner.has_type_stage_app(),
            Type::App(f, a) => f.has_type_stage_app() || a.has_type_stage_app(),
            Type::NominalVariant { tag: _, fields } => {
                fields.fields.values().any(|ty| ty.has_type_stage_app())
            }
            Type::Handle(cap) => cap.has_type_stage_app(),
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
            Type::Seq(elem) => elem.collect_all_vars(type_vars),
            Type::Map(key, val) => {
                key.collect_all_vars(type_vars);
                val.collect_all_vars(type_vars);
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
            Type::Handle(cap) => cap.collect_all_vars(type_vars),
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
            Type::Seq(elem) => elem.collect_all_vars_check_occurs(occurs_name, type_vars),
            Type::Map(key, val) => {
                let mut found = false;
                found |= key.collect_all_vars_check_occurs(occurs_name, type_vars);
                found |= val.collect_all_vars_check_occurs(occurs_name, type_vars);
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
            Type::Handle(cap) => cap.collect_all_vars_check_occurs(occurs_name, type_vars),
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
            Type::Seq(elem) => elem.collect_all_vars_vec(type_vars),
            Type::Map(key, val) => {
                key.collect_all_vars_vec(type_vars);
                val.collect_all_vars_vec(type_vars);
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
            Type::Handle(cap) => cap.collect_all_vars_vec(type_vars),
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
            Type::Handle(cap) => cap.collect_operator_names(operator_names),
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
                    },
                }
            }
            Type::Handle(cap) => Type::Handle(Box::new(Type::simplify_type(*cap))),
            _ => ty,
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
        Type::Handle(_) => 19,
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
        Type::TypeStageApp { .. } => 36,
        Type::NominalVariant { .. } => 37,
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
        // For complex types (Record, Function, Seq, Map, App, Handle), use Display representation
        // This is not ideal but ensures stability
        (Type::Record(_), Type::Record(_))
        | (Type::Function { .. }, Type::Function { .. })
        | (Type::Seq(_), Type::Seq(_))
        | (Type::Map(_, _), Type::Map(_, _))
        | (Type::App(_, _), Type::App(_, _))
        | (Type::Handle(_), Type::Handle(_)) => a.to_string().cmp(&b.to_string()),
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
        Type::TypeStageApp { fn_name: _, args } => {
            for arg in args {
                check_kind_wellformed(arg, kind_env, span)?;
            }
            Ok(())
        }
        Type::NominalVariant { tag: _, fields } => {
            for field_ty in fields.fields.values() {
                check_kind_wellformed(field_ty, kind_env, span)?;
            }
            Ok(())
        }
        Type::Handle(cap) => check_kind_wellformed(cap, kind_env, span),
        // All other types (Int, Str, Bool, literals, capabilities, etc.) are always well-kinded
        _ => Ok(()),
    }
}
