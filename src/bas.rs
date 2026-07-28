//! Boolean-Algebraic Subtyping (BAS) — RDNF normalization and emptiness checking.
//!
//! Implements the core BAS algorithms from:
//! - Parreaux, L. & Chau, C.Y. (2022). MLstruct: Principal type inference in a Boolean
//!   algebra of structural types. OOPSLA '22. doi:10.1145/3563304 — §2.2, §3.2, Fig. 6
//! - Chau, C.Y. & Parreaux, L. (2026). The simple essence of Boolean-algebraic subtyping.
//!   POPL '26. doi:10.1145/3776689 — §3.3, Theorem 7.6 (decidability)
//!
//! ## Algorithm Overview
//!
//! BAS subtyping: `A <: B` iff `A & ~B` is uninhabited.
//!
//! To check inhabitedness, we convert to Reduced Disjunctive Normal Form (RDNF):
//!   RDNF = Vec<Conjunction>           (disjuncts — type is inhabited if ANY conjunction is)
//!   Conjunction = Vec<SignedAtom>      (conjuncts — all must be simultaneously satisfiable)
//!   SignedAtom = Pos(Atom) | Neg(Atom) (positive or negative occurrence)
//!
//! Atoms are the irreducible structural components: primitives, single-field records,
//! functions, TyCon applications, NominalVariants, and Recursive types.
//!
//! ## Termination
//!
//! `to_rdnf` terminates by structural induction: each recursive call operates on a strict
//! sub-term of the input. There is NO depth limit — the transformation is total.
//!
//! `is_conjunction_empty` uses a depth limit for Recursive types (S-Exp coinduction) but
//! is otherwise structural.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::type_def::{extract_tycon_spine, unfold_once, Row, RowTail, TyConEnv, Type, Variance};

/// Maximum depth for atom subtype checking (coinductive Recursive type comparison).
const MAX_ATOM_SUBTYPE_DEPTH: usize = 256;

/// Maximum number of conjunctions allowed in an RDNF after distribution (cross-product).
///
/// The cross-product in `distribute()` produces |left| * |right| conjunctions. For deeply
/// nested intersections of unions, this can grow exponentially. This limit prevents
/// pathological blowup.
///
/// When exceeded, `distribute()` returns `vec![vec![]]` (Top RDNF = inhabited). This is
/// conservative-safe for the subtyping judgment: `A <: B` iff `A & ~B` is uninhabited.
/// Returning "inhabited" means `is_subtype` returns false (rejects the subtyping claim),
/// which is the safe direction for a type checker: when uncertain, reject. Accepting a
/// potentially ill-typed program (the previous behavior, B-590) is unsound.
const MAX_RDNF_CONJUNCTIONS: usize = 1024;

// ---------------------------------------------------------------------------
// RDNF types
// ---------------------------------------------------------------------------

/// An atom is an irreducible type that cannot be further decomposed by Boolean operations.
/// Multi-field records are NOT atoms — they decompose to intersections of single-field records.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Atom {
    /// Primitive types: Int, Float, Str, Bytes, Proxy, DirCap, NetCap, Uri,
    /// Timestamp, Duration, ClockCap, Timezone, QuicSession, Http2Session,
    /// Http3Session, QuicDatagramHandle, DatagramHandle
    Primitive(PrimitiveAtom),
    /// Literal types: IntLiteral(i64), StringLiteral(String)
    Literal(LiteralAtom),
    /// Single-field record: {key: Type}
    SingleFieldRecord { key: String, value: Box<Type> },
    /// Function type
    Function {
        params: Vec<(Option<String>, Type)>,
        ret: Box<Type>,
        typed_variadics: Vec<(String, Type)>,
        rest: Option<Box<(String, Type)>>,
        required_count: usize,
    },
    /// Type constructor application: App(TyCon("Seq"), Int) etc.
    TyCon(String),
    /// App of type constructor to arguments (full spine)
    App(Box<Type>, Box<Type>),
    /// Nominal variant: TyCon.Ctor { fields }
    NominalVariant {
        tycon: String,
        ctor: String,
        fields: Row,
    },
    /// Recursive type: mu var. body
    Recursive { var: String, body: Box<Type> },
    /// Type variable (unresolved inference variable)
    TypeVar(String, u32),
    /// Operator type variable (kind * -> *)
    Operator(String),
    /// TypeStageApp (stuck type-level computation)
    TypeStageApp { fn_name: String, args: Vec<Type> },
    /// Record that cannot be decomposed into single-field intersections:
    /// - Empty record {} (no fields)
    /// - Record with RowTail::Uniform (infinitary field constraint)
    ///
    /// These are treated as indivisible atoms; subtyping is handled directly.
    Record(Row),
}

/// Primitive type atoms
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveAtom {
    Int,
    Float,
    Str,
    Bytes,
    Proxy,
    DirCap,
    NetCap,
    Uri,
    Timestamp,
    Duration,
    ClockCap,
    Timezone,
    QuicSession,
    Http2Session,
    Http3Session,
    QuicDatagramHandle,
    DatagramHandle,
}

/// Literal type atoms
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LiteralAtom {
    IntLiteral(i64),
    StringLiteral(String),
}

/// A signed atom: positive (the type itself) or negative (its complement).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SignedAtom {
    Pos(Atom),
    Neg(Atom),
}

impl SignedAtom {
    pub fn negate(&self) -> SignedAtom {
        match self {
            SignedAtom::Pos(a) => SignedAtom::Neg(a.clone()),
            SignedAtom::Neg(a) => SignedAtom::Pos(a.clone()),
        }
    }
}

/// A conjunction (AND) of signed atoms. Represents a single "row" of the DNF.
/// The conjunction is satisfiable iff all atoms can be simultaneously inhabited.
pub type Conjunction = Vec<SignedAtom>;

/// Reduced Disjunctive Normal Form: a disjunction (OR) of conjunctions.
/// The type is inhabited iff ANY conjunction is non-empty (satisfiable).
pub type Rdnf = Vec<Conjunction>;

// ---------------------------------------------------------------------------
// T-1209: to_rdnf — Convert Type to Reduced Disjunctive Normal Form
// ---------------------------------------------------------------------------

/// Convert a Type to RDNF.
///
/// Transformation rules (structural induction — always terminates):
/// 1. Atom types → single positive conjunction: [[Pos(atom)]]
/// 2. Union(A, B) → to_rdnf(A) ++ to_rdnf(B)     (disjunction)
/// 3. Intersection(A, B) → cross-product of to_rdnf(A) and to_rdnf(B)  (distribution)
/// 4. Negation(A) → negate_rdnf(to_rdnf(A))       (De Morgan)
/// 5. Multi-field Record → intersection of single-field records (BAS decomposition)
/// 6. Top → [[]]  (empty conjunction = always satisfiable)
/// 7. Never → []   (empty disjunction = never satisfiable)
/// 8. Unknown → [[]]  (conservative: treated as Top for subtyping purposes)
/// 9. Error → []    (Error is uninhabited — nothing satisfies it)
pub fn to_rdnf(ty: &Type) -> Rdnf {
    match ty {
        // Top (Any) = universal type = empty conjunction (trivially satisfiable)
        Type::Any => vec![vec![]],

        // Never = empty type = empty disjunction (no conjunction is satisfiable)
        Type::Never => vec![],

        // Error = uninhabited sentinel
        Type::Error(_) => vec![],

        // Unknown: in BAS subtyping, Unknown is treated conservatively.
        // For `is_subtype`, the TypeVar guard fires first, and Unknown has its own guard.
        // In RDNF context (used internally), treat Unknown as Top (maximally permissive).
        Type::Unknown => vec![vec![]],

        // Union: disjunction of sub-RDNFs
        Type::Union(members) => {
            let mut result = Vec::new();
            for member in members {
                result.extend(to_rdnf(member));
            }
            result
        }

        // Intersection: distribute (cross-product of conjunctions)
        Type::Intersection(members) => {
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
        }

        // Negation: De Morgan's law
        // ~(A | B) = ~A & ~B  →  negate each disjunct, then distribute
        // ~(A & B) = ~A | ~B  →  negate each conjunct (atom), then combine as disjunction
        Type::Negation(inner) => {
            let inner_rdnf = to_rdnf(inner);
            negate_rdnf(&inner_rdnf)
        }

        // Multi-field Record: decompose to intersection of single-field records
        // {x: T1, y: T2} = {x: T1} & {y: T2}  (Parreaux & Chau 2022, §2.2.2)
        //
        // Special cases that are NOT decomposed (treated as atoms):
        // - Empty record {} — has structural identity distinct from Top
        // - Records with RowTail::Uniform — infinitary field constraint, cannot be expressed
        //   as a finite intersection of single-field records
        Type::Dict(row) => {
            // Records with Uniform tails cannot be decomposed — treat as atom
            if !matches!(row.tail, RowTail::Empty) {
                return vec![vec![SignedAtom::Pos(Atom::Record(row.clone()))]];
            }

            if row.fields.is_empty() {
                // Empty record {} with Empty tail. Under BAS, {} means "any record" (open).
                // Treat as an atom to preserve structural identity (functions are not {}).
                vec![vec![SignedAtom::Pos(Atom::Record(row.clone()))]]
            } else if row.fields.len() == 1 {
                // Single-field record: already an atom
                let (key, val) = row.fields.iter().next().unwrap();
                vec![vec![SignedAtom::Pos(Atom::SingleFieldRecord {
                    key: key.clone(),
                    value: Box::new(val.clone()),
                })]]
            } else {
                // Multi-field record: intersection of single-field records
                let single_fields: Vec<Type> = row
                    .fields
                    .iter()
                    .map(|(k, v)| {
                        Type::Dict(Row {
                            fields: {
                                let mut m = IndexMap::new();
                                m.insert(k.clone(), v.clone());
                                m
                            },
                            tail: RowTail::Empty,
                        })
                    })
                    .collect();
                // Recurse through intersection
                let intersection = Type::Intersection(single_fields);
                to_rdnf(&intersection)
            }
        }

        // Function: atom
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
        } => vec![vec![SignedAtom::Pos(Atom::Function {
            params: params.clone(),
            ret: ret.clone(),
            typed_variadics: typed_variadics.clone(),
            rest: rest.clone(),
            required_count: *required_count,
        })]],

        // Primitives: atoms
        Type::Int => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))]],
        Type::Float => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Float))]],
        Type::Str => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Str))]],
        Type::Bytes => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Bytes))]],
        Type::Proxy => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Proxy))]],
        Type::DirCap => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::DirCap,
        ))]],
        Type::NetCap => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::NetCap,
        ))]],
        Type::Uri => vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Uri))]],
        Type::Timestamp => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::Timestamp,
        ))]],
        Type::Duration => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::Duration,
        ))]],
        Type::ClockCap => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::ClockCap,
        ))]],
        Type::Timezone => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::Timezone,
        ))]],
        Type::QuicSession => {
            vec![vec![SignedAtom::Pos(Atom::Primitive(
                PrimitiveAtom::QuicSession,
            ))]]
        }
        Type::Http2Session => {
            vec![vec![SignedAtom::Pos(Atom::Primitive(
                PrimitiveAtom::Http2Session,
            ))]]
        }
        Type::Http3Session => {
            vec![vec![SignedAtom::Pos(Atom::Primitive(
                PrimitiveAtom::Http3Session,
            ))]]
        }
        Type::QuicDatagramHandle => vec![vec![SignedAtom::Pos(Atom::Primitive(
            PrimitiveAtom::QuicDatagramHandle,
        ))]],
        Type::DatagramHandle => {
            vec![vec![SignedAtom::Pos(Atom::Primitive(
                PrimitiveAtom::DatagramHandle,
            ))]]
        }

        // Literal types: atoms
        Type::IntLiteral(n) => vec![vec![SignedAtom::Pos(Atom::Literal(
            LiteralAtom::IntLiteral(*n),
        ))]],
        Type::StringLiteral(s) => vec![vec![SignedAtom::Pos(Atom::Literal(
            LiteralAtom::StringLiteral(s.clone()),
        ))]],

        // Type constructor: atom
        Type::TyCon(name) => vec![vec![SignedAtom::Pos(Atom::TyCon(name.clone()))]],
        Type::TyConResolved(name, _arc) => vec![vec![SignedAtom::Pos(Atom::TyCon(name.clone()))]],

        // Type application: atom
        Type::App(f, a) => vec![vec![SignedAtom::Pos(Atom::App(f.clone(), a.clone()))]],

        // NominalVariant: atom
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => vec![vec![SignedAtom::Pos(Atom::NominalVariant {
            tycon: tycon.clone(),
            ctor: ctor.clone(),
            fields: fields.clone(),
        })]],

        // TypeVar: atom (inference variable — handled specially in is_subtype guards)
        Type::Var(name, level) => {
            vec![vec![SignedAtom::Pos(Atom::TypeVar(name.clone(), *level))]]
        }

        // Operator: atom
        Type::Operator(name) => {
            vec![vec![SignedAtom::Pos(Atom::Operator(name.clone()))]]
        }

        // TypeStageApp: atom (stuck computation)
        Type::StageApp { fn_name, args } => vec![vec![SignedAtom::Pos(Atom::TypeStageApp {
            fn_name: fn_name.clone(),
            args: args.clone(),
        })]],

        // Recursive: atom (coinductive — handled by is_atom_subtype with sigma)
        Type::Recursive { var, body } => vec![vec![SignedAtom::Pos(Atom::Recursive {
            var: var.clone(),
            body: body.clone(),
        })]],
    }
}

/// Distribute two RDNFs (cross-product for intersection).
/// (A1 | A2) & (B1 | B2) = (A1 & B1) | (A1 & B2) | (A2 & B1) | (A2 & B2)
///
/// Guarded by `MAX_RDNF_CONJUNCTIONS`: if the result would exceed the limit, returns
/// `vec![vec![]]` (Top RDNF = inhabited). This is the conservative-safe direction for
/// a type checker: when uncertain, assume the type IS inhabited (i.e., the difference
/// A & ~B is non-empty), causing `is_subtype` to return false (reject). The alternative
/// (returning empty = uninhabited) would cause `is_subtype` to return true, accepting
/// potentially ill-typed programs. B-590.
fn distribute(left: &Rdnf, right: &Rdnf) -> Rdnf {
    // Special cases for empty disjunctions
    if left.is_empty() || right.is_empty() {
        // Never & T = Never, T & Never = Never
        return vec![];
    }
    let product_size = left.len().saturating_mul(right.len());
    if product_size > MAX_RDNF_CONJUNCTIONS {
        // B-590: Cross-product would exceed limit — return Top (conservative: "inhabited").
        // This causes is_subtype to reject (return false) when uncertain, which is the
        // safe direction for a type checker.
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
///
/// ~(C1 | C2 | ... | Cn) = ~C1 & ~C2 & ... & ~Cn
///
/// Where ~Ci (negation of a conjunction) = ~a1 | ~a2 | ... | ~am
///   (each negated atom becomes a separate single-atom conjunction, forming a disjunction)
///
/// The result is the distributed intersection of all negated conjunctions.
fn negate_rdnf(rdnf: &Rdnf) -> Rdnf {
    if rdnf.is_empty() {
        // ~Never = Top = empty conjunction (trivially satisfiable)
        return vec![vec![]];
    }

    // Start with Top (identity for intersection)
    let mut result: Rdnf = vec![vec![]];

    for conjunction in rdnf {
        if conjunction.is_empty() {
            // ~Top = Never (empty disjunction)
            return vec![];
        }
        // ~(a1 & a2 & ... & am) = ~a1 | ~a2 | ... | ~am  (De Morgan)
        // Each negated atom becomes a single-element conjunction in a disjunction
        let negated_conj: Rdnf = conjunction.iter().map(|atom| vec![atom.negate()]).collect();
        // Distribute: result & negated_conj.
        // distribute() may trigger MAX_RDNF_CONJUNCTIONS and return Top (vec![vec![]]).
        // B-590: Top in this context means the negation result is inhabited (non-empty),
        // which causes is_subtype to return false — rejecting uncertain subtyping claims.
        // This is the conservative-safe direction for a type checker.
        result = distribute(&result, &negated_conj);
    }

    result
}

// ---------------------------------------------------------------------------
// T-1210: is_atom_subtype — Per-atom structural subtype comparison
// ---------------------------------------------------------------------------

/// Check whether atom `sub` is a subtype of atom `sup`.
///
/// This handles structural decomposition within atoms:
/// - Literal promotions: IntLiteral(n) <: Int, StringLiteral(s) <: Str
/// - Single-field record covariance: {k: A} <: {k: B} when A <: B
/// - Function: contravariant params, covariant return
/// - App: variance-directed via TyConEnv
/// - Recursive: coinductive via sigma (S-Assum/S-Exp)
///
/// TypeVar is NOT handled here — TypeVars are handled by the top-level is_subtype guards.
pub fn is_atom_subtype(
    sub: &Atom,
    sup: &Atom,
    tycon_env: Option<&TyConEnv>,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if depth >= MAX_ATOM_SUBTYPE_DEPTH {
        return false;
    }

    match (sub, sup) {
        // Same atom → reflexive
        (a, b) if a == b => true,

        // Literal promotions
        (Atom::Literal(LiteralAtom::IntLiteral(_)), Atom::Primitive(PrimitiveAtom::Int)) => true,
        (Atom::Literal(LiteralAtom::StringLiteral(_)), Atom::Primitive(PrimitiveAtom::Str)) => true,

        // Single-field record: covariant in value type, key must match exactly
        (
            Atom::SingleFieldRecord { key: k1, value: v1 },
            Atom::SingleFieldRecord { key: k2, value: v2 },
        ) => k1 == k2 && Type::is_subtype_bas(v1, v2, tycon_env, sigma),

        // Function: contravariant params, covariant return
        (
            Atom::Function {
                params: sub_p,
                ret: sub_r,
                typed_variadics: sub_tv,
                rest: sub_rest,
                required_count: _,
            },
            Atom::Function {
                params: sup_p,
                ret: sup_r,
                typed_variadics: sup_tv,
                rest: sup_rest,
                required_count: _,
            },
        ) => {
            // Any-function special cases (zero-param variadic)
            let sub_is_variadic = !sub_tv.is_empty() || sub_rest.is_some();
            let sup_is_variadic = !sup_tv.is_empty() || sup_rest.is_some();
            let sub_is_any = sub_p.is_empty() && sub_is_variadic;
            let sup_is_any = sup_p.is_empty() && sup_is_variadic;

            if sub_is_any && sup_is_any {
                return Type::is_subtype_bas(sub_r, sup_r, tycon_env, sigma);
            }
            if sup_is_any && !sub_p.is_empty() {
                return true;
            }
            if sub_is_any {
                return false;
            }

            // typed_variadics and rest are subtyped element-wise (contravariant),
            // consistent with how fixed params are handled. Value equality would
            // reject valid subtype pairs like Fn[...xs@Seq[Number]] >: Fn[...xs@Seq[Int]].
            let tv_subtype = sub_tv.len() == sup_tv.len()
                && sub_tv
                    .iter()
                    .zip(sup_tv.iter())
                    .all(|((_, sub_t), (_, sup_t))| {
                        // Contravariant: sup_tv_param <: sub_tv_param
                        Type::is_subtype_bas(sup_t, sub_t, tycon_env, sigma)
                    });
            let rest_subtype = match (sub_rest, sup_rest) {
                (Some(sr), Some(pr)) => {
                    // Contravariant: sup_rest_param <: sub_rest_param
                    Type::is_subtype_bas(&pr.1, &sr.1, tycon_env, sigma)
                }
                (None, None) => true,
                _ => false,
            };
            sub_is_variadic == sup_is_variadic
                && tv_subtype
                && rest_subtype
                && sub_p.len() == sup_p.len()
                && sub_p
                    .iter()
                    .zip(sup_p.iter())
                    .all(|((_sp_name, sp_ty), (_pp_name, pp_ty))| {
                        // Contravariant: sup_param <: sub_param
                        Type::is_subtype_bas(pp_ty, sp_ty, tycon_env, sigma)
                    })
                && Type::is_subtype_bas(sub_r, sup_r, tycon_env, sigma)
        }

        // App: variance-directed
        (Atom::App(f1, a1), Atom::App(f2, a2)) => {
            let sub_ty = Type::App(f1.clone(), a1.clone());
            let sup_ty = Type::App(f2.clone(), a2.clone());
            if let (Some((name1, args1)), Some((name2, args2))) =
                (extract_tycon_spine(&sub_ty), extract_tycon_spine(&sup_ty))
            {
                if name1 == name2 && args1.len() == args2.len() {
                    if let Some(env) = tycon_env {
                        if let Some(def) = env.get(name1) {
                            for (i, (sub_arg, sup_arg)) in
                                args1.iter().zip(args2.iter()).enumerate()
                            {
                                let var =
                                    def.variance.get(i).copied().unwrap_or(Variance::Invariant);
                                let ok = match var {
                                    Variance::Covariant => {
                                        Type::is_subtype_bas(sub_arg, sup_arg, tycon_env, sigma)
                                    }
                                    Variance::Contravariant => {
                                        Type::is_subtype_bas(sup_arg, sub_arg, tycon_env, sigma)
                                    }
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
                    // No env or no def: conservative invariant fallback
                    return args1.iter().zip(args2.iter()).all(|(a, b)| a == b);
                }
            }
            // Different TyCons or non-TyCon App: structural recursion
            Type::is_subtype_bas(f1, f2, tycon_env, sigma)
                && Type::is_subtype_bas(a1, a2, tycon_env, sigma)
        }

        // TyCon: nominal equality
        (Atom::TyCon(n1), Atom::TyCon(n2)) => n1 == n2,

        // TyCon("Dict") vs open Record: TyCon("Dict") is the nominal representation of
        // the structural dict type. Any Value::Dict satisfies any open record constraint,
        // so TyCon("Dict") ≤ Record({}, Uniform{V}) for any V ≥ Any.
        (Atom::TyCon(name), Atom::Record(r2)) if name == "Dict" => {
            if r2.fields.is_empty() {
                if let RowTail::Uniform { value: sup_v, .. } = &r2.tail {
                    return Type::is_subtype_bas(&Type::Any, sup_v, tycon_env, sigma);
                }
            }
            false
        }

        // NominalVariant: tycon and ctor must match, fields are covariant
        (
            Atom::NominalVariant {
                tycon: tycon1,
                ctor: ctor1,
                fields: f1,
            },
            Atom::NominalVariant {
                tycon: tycon2,
                ctor: ctor2,
                fields: f2,
            },
        ) => {
            if tycon1 != tycon2 || ctor1 != ctor2 {
                return false;
            }
            // Width subtyping on fields: sup must be a subset
            for (k, sup_ty) in &f2.fields {
                match f1.fields.get(k) {
                    Some(sub_ty) => {
                        if !Type::is_subtype_bas(sub_ty, sup_ty, tycon_env, sigma) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }

        // Recursive: coinductive (S-Assum/S-Exp)
        (Atom::Recursive { var: v1, body: b1 }, Atom::Recursive { var: v2, body: b2 }) => {
            let key = (v1.clone(), v2.clone());
            if sigma.contains(&key) {
                return true; // S-Assum: coinductive hypothesis
            }
            sigma.insert(key);
            // S-Exp: unfold both and recurse
            let sub_unfolded = unfold_once(&Type::Recursive {
                var: v1.clone(),
                body: b1.clone(),
            });
            let sup_unfolded = unfold_once(&Type::Recursive {
                var: v2.clone(),
                body: b2.clone(),
            });
            Type::is_subtype_bas(&sub_unfolded, &sup_unfolded, tycon_env, sigma)
        }

        // Recursive on one side only: unfold and recurse
        (Atom::Recursive { var, body }, _) => {
            let unfolded = unfold_once(&Type::Recursive {
                var: var.clone(),
                body: body.clone(),
            });
            let sup_ty = atom_to_type(sup);
            Type::is_subtype_bas(&unfolded, &sup_ty, tycon_env, sigma)
        }
        (_, Atom::Recursive { var, body }) => {
            let sub_ty = atom_to_type(sub);
            let unfolded = unfold_once(&Type::Recursive {
                var: var.clone(),
                body: body.clone(),
            });
            Type::is_subtype_bas(&sub_ty, &unfolded, tycon_env, sigma)
        }

        // Operator: nominal equality
        (Atom::Operator(n1), Atom::Operator(n2)) => n1 == n2,

        // TypeStageApp: conservative — not a subtype of anything until reduced
        (Atom::TypeStageApp { .. }, _) | (_, Atom::TypeStageApp { .. }) => false,

        // Record atoms: empty records and Uniform-tailed records
        // These are handled via direct Type-level subtyping delegation.
        (Atom::Record(r1), Atom::Record(r2)) => {
            // Delegate to full record subtyping logic:
            // All fields in r2 must be present in r1 with compatible types.
            for (k, sup_ty) in &r2.fields {
                match r1.fields.get(k) {
                    Some(sub_ty) => {
                        if !Type::is_subtype_bas(sub_ty, sup_ty, tycon_env, sigma) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            // Tail subtyping
            match (&r1.tail, &r2.tail) {
                (RowTail::Empty, RowTail::Empty) => true,
                (
                    sub_tail,
                    RowTail::Uniform {
                        key: sup_key,
                        value: sup_v,
                    },
                ) => {
                    for sub_field_ty in r1.fields.values() {
                        if !Type::is_subtype_bas(sub_field_ty, sup_v, tycon_env, sigma) {
                            return false;
                        }
                    }
                    if let RowTail::Uniform {
                        key: sub_key,
                        value: sub_v,
                    } = sub_tail
                    {
                        if !Type::is_subtype_bas(sub_v, sup_v, tycon_env, sigma) {
                            return false;
                        }
                        if let Some(sup_k) = sup_key {
                            match sub_key {
                                Some(sub_k) => {
                                    if !Type::is_subtype_bas(sub_k, sup_k, tycon_env, sigma) {
                                        return false;
                                    }
                                }
                                None => return false,
                            }
                        }
                    }
                    true
                }
                (RowTail::Uniform { .. }, RowTail::Empty) => false,
            }
        }

        // Record atom vs SingleFieldRecord: Record contains the field
        (Atom::Record(rec), Atom::SingleFieldRecord { key, value }) => {
            // Record <: {k: V} iff Record has field k with type <: V
            match rec.fields.get(key) {
                Some(field_ty) => Type::is_subtype_bas(field_ty, value, tycon_env, sigma),
                None => {
                    // Check Uniform tail
                    if let RowTail::Uniform { value: v, .. } = &rec.tail {
                        Type::is_subtype_bas(v, value, tycon_env, sigma)
                    } else {
                        false
                    }
                }
            }
        }
        // SingleFieldRecord vs Record atom: {k: V} <: Record(r2) iff r2 accepts field k with type V.
        // Case 1: r2 explicitly has field k — compare value types.
        // Case 2: r2 has a Uniform tail (open dict) — any field is accepted; compare V against sup_v.
        (Atom::SingleFieldRecord { key, value }, Atom::Record(r2)) => {
            if let Some(r2_field_ty) = r2.fields.get(key.as_str()) {
                Type::is_subtype_bas(value, r2_field_ty, tycon_env, sigma)
            } else if let RowTail::Uniform { value: sup_v, .. } = &r2.tail {
                Type::is_subtype_bas(value, sup_v, tycon_env, sigma)
            } else {
                false
            }
        }

        // Record vs primitive/function/variant/literal → disjoint, never subtypes
        (Atom::Record(_), Atom::Primitive(_))
        | (Atom::Primitive(_), Atom::Record(_))
        | (Atom::Record(_), Atom::Function { .. })
        | (Atom::Function { .. }, Atom::Record(_))
        | (Atom::Record(_), Atom::NominalVariant { .. })
        | (Atom::NominalVariant { .. }, Atom::Record(_))
        | (Atom::Record(_), Atom::Literal(_))
        | (Atom::Literal(_), Atom::Record(_))
        | (Atom::Record(_), Atom::App(_, _))
        | (Atom::App(_, _), Atom::Record(_))
        | (Atom::Record(_), Atom::TyCon(_))
        | (Atom::TyCon(_), Atom::Record(_)) => false,

        // NominalVariant <: TyCon: the variant is a member of the TyCon family.
        (Atom::NominalVariant { tycon, .. }, Atom::TyCon(n)) => {
            if let Some(env) = tycon_env {
                if let Some(def) = env.get(n.as_str()) {
                    let variant_ty = atom_to_type(sub);
                    return Type::is_subtype_bas(&variant_ty, &def.body, tycon_env, sigma);
                }
            }
            tycon == n
        }

        // NominalVariant vs non-NominalVariant (other kinds): never subtypes
        (Atom::NominalVariant { .. }, _) | (_, Atom::NominalVariant { .. }) => false,

        // Otherwise: different kinds of atoms are never subtypes
        _ => false,
    }
}

/// Convert an Atom back to a Type (for recursive calls through the main is_subtype).
fn atom_to_type(atom: &Atom) -> Type {
    match atom {
        Atom::Primitive(p) => match p {
            PrimitiveAtom::Int => Type::Int,
            PrimitiveAtom::Float => Type::Float,
            PrimitiveAtom::Str => Type::Str,
            PrimitiveAtom::Bytes => Type::Bytes,
            PrimitiveAtom::Proxy => Type::Proxy,
            PrimitiveAtom::DirCap => Type::DirCap,
            PrimitiveAtom::NetCap => Type::NetCap,
            PrimitiveAtom::Uri => Type::Uri,
            PrimitiveAtom::Timestamp => Type::Timestamp,
            PrimitiveAtom::Duration => Type::Duration,
            PrimitiveAtom::ClockCap => Type::ClockCap,
            PrimitiveAtom::Timezone => Type::Timezone,
            PrimitiveAtom::QuicSession => Type::QuicSession,
            PrimitiveAtom::Http2Session => Type::Http2Session,
            PrimitiveAtom::Http3Session => Type::Http3Session,
            PrimitiveAtom::QuicDatagramHandle => Type::QuicDatagramHandle,
            PrimitiveAtom::DatagramHandle => Type::DatagramHandle,
        },
        Atom::Literal(l) => match l {
            LiteralAtom::IntLiteral(n) => Type::IntLiteral(*n),
            LiteralAtom::StringLiteral(s) => Type::StringLiteral(s.clone()),
        },
        Atom::SingleFieldRecord { key, value } => Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert(key.clone(), *value.clone());
                m
            },
            tail: RowTail::Empty,
        }),
        Atom::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
        } => Type::Function {
            params: params.clone(),
            ret: ret.clone(),
            typed_variadics: typed_variadics.clone(),
            rest: rest.clone(),
            required_count: *required_count,
        },
        Atom::TyCon(name) => Type::TyCon(name.clone()),
        Atom::App(f, a) => Type::App(f.clone(), a.clone()),
        Atom::NominalVariant {
            tycon,
            ctor,
            fields,
        } => Type::NominalVariant {
            tycon: tycon.clone(),
            ctor: ctor.clone(),
            fields: fields.clone(),
        },
        Atom::Recursive { var, body } => Type::Recursive {
            var: var.clone(),
            body: body.clone(),
        },
        Atom::TypeVar(name, level) => Type::Var(name.clone(), *level),
        Atom::Operator(name) => Type::Operator(name.clone()),
        Atom::TypeStageApp { fn_name, args } => Type::StageApp {
            fn_name: fn_name.clone(),
            args: args.clone(),
        },
        Atom::Record(row) => Type::Dict(row.clone()),
    }
}

/// Convert an RDNF back to a `Type`.
///
/// Each conjunction becomes an Intersection of signed atoms (positive atoms are kept,
/// negative atoms are wrapped in `Type::Negation`). The resulting conjunctions are
/// unioned into a single `Type::Union`. Identity cases: empty RDNF → `Type::Never`;
/// single conjunction with a single positive atom → the atom type directly.
///
/// Used by `constrain()` in type_unify.rs when pushing bounds to `state.bounds`:
/// the RDNF form of a bound is normalized (simplifications applied) and then
/// flattened back to a `Type` before accumulation.
pub fn flatten_rdnf_to_type(rdnf: Rdnf) -> Type {
    if rdnf.is_empty() {
        return Type::Never;
    }

    let conjunctions: Vec<Type> = rdnf
        .into_iter()
        .map(|conj| {
            if conj.is_empty() {
                // Empty conjunction = Any/Top (satisfied by everything)
                return Type::Any;
            }
            let members: Vec<Type> = conj
                .into_iter()
                .map(|signed| match signed {
                    SignedAtom::Pos(atom) => atom_to_type(&atom),
                    SignedAtom::Neg(atom) => Type::Negation(Box::new(atom_to_type(&atom))),
                })
                .collect();
            if members.len() == 1 {
                members.into_iter().next().unwrap()
            } else {
                Type::normalize_intersection(members)
            }
        })
        .collect();

    if conjunctions.len() == 1 {
        conjunctions.into_iter().next().unwrap()
    } else {
        Type::normalize_union(conjunctions)
    }
}

// ---------------------------------------------------------------------------
// T-1211: Emptiness checking — is a conjunction/RDNF uninhabited?
// ---------------------------------------------------------------------------

/// Check if an RDNF is empty (uninhabited).
/// A disjunction is empty iff ALL conjunctions are empty.
///
/// Each conjunction is checked with a fresh clone of `sigma` so that coinductive
/// assumptions accumulated during one conjunction's emptiness check do not leak into
/// another. The disjuncts are independent alternatives — an assumption that recursive
/// types (mu a. A) <: (mu b. B) made while checking conjunction C1 is only valid within
/// C1's proof tree, not C2's.
pub fn is_rdnf_empty(
    rdnf: &Rdnf,
    tycon_env: Option<&TyConEnv>,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if rdnf.is_empty() {
        return true; // No disjuncts → Never
    }
    rdnf.iter().all(|conj| {
        let mut conj_sigma = sigma.clone();
        is_conjunction_empty(conj, tycon_env, &mut conj_sigma)
    })
}

/// Check if a conjunction of signed atoms is empty (uninhabited).
///
/// A conjunction is empty if EITHER:
/// 1. **Component disjointness**: two positive atoms of incompatible kinds exist
///    (e.g., Pos(Int) and Pos(Str) — no value can be both Int and Str)
/// 2. **Positive subsumed by negative**: some positive atom is subsumed by a negative atom
///    (e.g., Pos(Int) and Neg(Int) — nothing is both Int and ~Int)
///    More generally: Pos(A) and Neg(B) where A <: B means A & ~B = Never.
///
/// An empty conjunction [] (no constraints) is trivially satisfiable (= Top).
fn is_conjunction_empty(
    conj: &Conjunction,
    tycon_env: Option<&TyConEnv>,
    sigma: &mut HashSet<(String, String)>,
) -> bool {
    if conj.is_empty() {
        return false; // Empty conjunction = Top = inhabited
    }

    // Partition into positive and negative atoms
    let positives: Vec<&Atom> = conj
        .iter()
        .filter_map(|sa| match sa {
            SignedAtom::Pos(a) => Some(a),
            _ => None,
        })
        .collect();

    let negatives: Vec<&Atom> = conj
        .iter()
        .filter_map(|sa| match sa {
            SignedAtom::Neg(a) => Some(a),
            _ => None,
        })
        .collect();

    // Condition 1: Positive-atom component disjointness.
    // Check every pair of positive atoms for incompatibility.
    for i in 0..positives.len() {
        for j in (i + 1)..positives.len() {
            if atoms_are_disjoint(positives[i], positives[j], tycon_env) {
                return true; // Conjunction is empty — incompatible positives
            }
        }
    }

    // Condition 2: Positive atom subsumed by negative atom.
    // For each positive atom P and negative atom N:
    //   if P <: N, then P & ~N = Never, so the conjunction is empty.
    for pos in &positives {
        for neg in &negatives {
            if is_atom_subtype(pos, neg, tycon_env, 0, sigma) {
                return true; // P ∈ N → P & ~N = ∅
            }
        }
    }

    false
}

/// Extract the primitive atom from a Type if it is a direct primitive.
///
/// Returns Some(p) for Type::Int, Type::Float, etc. Returns None for compound types.
/// Used by `atoms_are_disjoint` to determine same-key record value disjointness without
/// threading tycon_env/sigma through the function.
fn type_as_primitive(ty: &Type) -> Option<PrimitiveAtom> {
    match ty {
        Type::Int => Some(PrimitiveAtom::Int),
        Type::Float => Some(PrimitiveAtom::Float),
        Type::Str => Some(PrimitiveAtom::Str),
        Type::Bytes => Some(PrimitiveAtom::Bytes),
        Type::Proxy => Some(PrimitiveAtom::Proxy),
        Type::DirCap => Some(PrimitiveAtom::DirCap),
        Type::NetCap => Some(PrimitiveAtom::NetCap),
        Type::Uri => Some(PrimitiveAtom::Uri),
        Type::Timestamp => Some(PrimitiveAtom::Timestamp),
        Type::Duration => Some(PrimitiveAtom::Duration),
        Type::ClockCap => Some(PrimitiveAtom::ClockCap),
        Type::Timezone => Some(PrimitiveAtom::Timezone),
        Type::QuicSession => Some(PrimitiveAtom::QuicSession),
        Type::Http2Session => Some(PrimitiveAtom::Http2Session),
        Type::Http3Session => Some(PrimitiveAtom::Http3Session),
        Type::QuicDatagramHandle => Some(PrimitiveAtom::QuicDatagramHandle),
        Type::DatagramHandle => Some(PrimitiveAtom::DatagramHandle),
        _ => None,
    }
}

/// Check if two atoms are disjoint (no value can inhabit both simultaneously).
///
/// This is the structural disjointness oracle for the BAS emptiness check.
/// It mirrors `types_are_disjoint` but operates on atoms.
fn atoms_are_disjoint(a: &Atom, b: &Atom, tycon_env: Option<&TyConEnv>) -> bool {
    match (a, b) {
        // Same atom → not disjoint
        (x, y) if x == y => false,

        // Different primitives → disjoint
        (Atom::Primitive(p1), Atom::Primitive(p2)) => p1 != p2,

        // Literal vs different primitive → disjoint
        (Atom::Literal(LiteralAtom::IntLiteral(_)), Atom::Primitive(p))
        | (Atom::Primitive(p), Atom::Literal(LiteralAtom::IntLiteral(_))) => {
            !matches!(p, PrimitiveAtom::Int)
        }
        (Atom::Literal(LiteralAtom::StringLiteral(_)), Atom::Primitive(p))
        | (Atom::Primitive(p), Atom::Literal(LiteralAtom::StringLiteral(_))) => {
            !matches!(p, PrimitiveAtom::Str)
        }

        // Different literal kinds → disjoint
        (
            Atom::Literal(LiteralAtom::IntLiteral(_)),
            Atom::Literal(LiteralAtom::StringLiteral(_)),
        )
        | (
            Atom::Literal(LiteralAtom::StringLiteral(_)),
            Atom::Literal(LiteralAtom::IntLiteral(_)),
        ) => true,

        // Different IntLiterals → disjoint
        (Atom::Literal(LiteralAtom::IntLiteral(a)), Atom::Literal(LiteralAtom::IntLiteral(b))) => {
            a != b
        }

        // Different StringLiterals → disjoint
        (
            Atom::Literal(LiteralAtom::StringLiteral(a)),
            Atom::Literal(LiteralAtom::StringLiteral(b)),
        ) => a != b,

        // Record vs primitive/function/variant → disjoint
        (Atom::SingleFieldRecord { .. }, Atom::Primitive(_))
        | (Atom::Primitive(_), Atom::SingleFieldRecord { .. }) => true,
        (Atom::SingleFieldRecord { .. }, Atom::Function { .. })
        | (Atom::Function { .. }, Atom::SingleFieldRecord { .. }) => true,
        (Atom::SingleFieldRecord { .. }, Atom::NominalVariant { .. })
        | (Atom::NominalVariant { .. }, Atom::SingleFieldRecord { .. }) => true,
        (Atom::SingleFieldRecord { .. }, Atom::Literal(_))
        | (Atom::Literal(_), Atom::SingleFieldRecord { .. }) => true,

        // Two single-field records:
        //   - Different keys → NOT disjoint: {x:T} and {y:U} can coexist as {x:T, y:U}.
        //     (C-252: "single-field records with different fields: NOT disjoint")
        //   - Same key, compatible value types → NOT disjoint: both constrain the same field.
        //   - Same key, incompatible primitive value types → DISJOINT: a value cannot have
        //     field k typed simultaneously as Int and Str (or any two distinct primitives).
        //     For complex value types we stay conservative (return false) to avoid needing
        //     tycon_env/sigma here; the is_conjunction_empty condition 2 (positive subsumed
        //     by negative) handles those cases through is_atom_subtype.
        (
            Atom::SingleFieldRecord { key: k1, value: v1 },
            Atom::SingleFieldRecord { key: k2, value: v2 },
        ) => {
            if k1 != k2 {
                // Different keys: not disjoint under BAS open-record semantics.
                false
            } else {
                // Same key: disjoint iff value types are incompatible primitives.
                match (type_as_primitive(v1), type_as_primitive(v2)) {
                    (Some(p1), Some(p2)) => p1 != p2,
                    // One or both values are not simple primitives: conservative.
                    _ => false,
                }
            }
        }

        // Function vs primitive/record/variant → disjoint
        (Atom::Function { .. }, Atom::Primitive(_))
        | (Atom::Primitive(_), Atom::Function { .. }) => true,
        (Atom::Function { .. }, Atom::NominalVariant { .. })
        | (Atom::NominalVariant { .. }, Atom::Function { .. }) => true,
        (Atom::Function { .. }, Atom::Literal(_)) | (Atom::Literal(_), Atom::Function { .. }) => {
            true
        }
        (Atom::Function { .. }, Atom::App(_, _)) | (Atom::App(_, _), Atom::Function { .. }) => true,

        // NominalVariant: different tags → disjoint
        (
            Atom::NominalVariant {
                tycon: tycon1,
                ctor: ctor1,
                ..
            },
            Atom::NominalVariant {
                tycon: tycon2,
                ctor: ctor2,
                ..
            },
        ) => tycon1 != tycon2 || ctor1 != ctor2,

        // NominalVariant vs primitives
        (Atom::NominalVariant { .. }, Atom::Primitive(_))
        | (Atom::Primitive(_), Atom::NominalVariant { .. }) => true,
        (Atom::NominalVariant { .. }, Atom::Literal(_))
        | (Atom::Literal(_), Atom::NominalVariant { .. }) => true,
        (Atom::NominalVariant { .. }, Atom::App(_, _))
        | (Atom::App(_, _), Atom::NominalVariant { .. }) => true,

        // App vs primitive/literal
        (Atom::App(_, _), Atom::Primitive(_)) | (Atom::Primitive(_), Atom::App(_, _)) => true,
        (Atom::App(_, _), Atom::Literal(_)) | (Atom::Literal(_), Atom::App(_, _)) => true,
        // App vs single-field record
        (Atom::App(_, _), Atom::SingleFieldRecord { .. })
        | (Atom::SingleFieldRecord { .. }, Atom::App(_, _)) => true,

        // Record vs primitive/function/variant/literal → disjoint
        (Atom::Record(_), Atom::Primitive(_)) | (Atom::Primitive(_), Atom::Record(_)) => true,
        (Atom::Record(_), Atom::Function { .. }) | (Atom::Function { .. }, Atom::Record(_)) => true,
        (Atom::Record(_), Atom::NominalVariant { .. })
        | (Atom::NominalVariant { .. }, Atom::Record(_)) => true,
        (Atom::Record(_), Atom::Literal(_)) | (Atom::Literal(_), Atom::Record(_)) => true,
        (Atom::Record(_), Atom::App(_, _)) | (Atom::App(_, _), Atom::Record(_)) => true,
        // Record vs TyCon: opaque TyCons (Handle, DirCap, etc.) are disjoint from Records.
        // Exception: TyCon("Dict") is the nominal Dict type whose runtime representation IS
        // Value::Dict — a structural dict. TyCon("Dict") and open Record types overlap.
        (Atom::Record(_), Atom::TyCon(n)) | (Atom::TyCon(n), Atom::Record(_)) => n != "Dict",
        // Record vs SingleFieldRecord: not disjoint (Record could contain the field)
        (Atom::Record(_), Atom::SingleFieldRecord { .. })
        | (Atom::SingleFieldRecord { .. }, Atom::Record(_)) => false,
        // Two Record atoms: not disjoint in general (conservative)
        (Atom::Record(_), Atom::Record(_)) => false,

        // TypeVar: conservative — not disjoint from anything (unresolved)
        (Atom::TypeVar(_, _), _) | (_, Atom::TypeVar(_, _)) => false,

        // Operator: conservative
        (Atom::Operator(_), _) | (_, Atom::Operator(_)) => false,

        // TypeStageApp: conservative
        (Atom::TypeStageApp { .. }, _) | (_, Atom::TypeStageApp { .. }) => false,

        // Recursive: conservative (would need to unfold)
        (Atom::Recursive { .. }, _) | (_, Atom::Recursive { .. }) => false,

        // App: different TyCon roots → disjoint (Seq[A] vs Map[K,V])
        (Atom::App(f1, _), Atom::App(f2, _)) => {
            let sub_ty = Type::App(f1.clone(), Box::new(Type::Never));
            let sup_ty = Type::App(f2.clone(), Box::new(Type::Never));
            if let (Some((name1, _)), Some((name2, _))) =
                (extract_tycon_spine(&sub_ty), extract_tycon_spine(&sup_ty))
            {
                name1 != name2
            } else {
                false // Conservative: non-TyCon App heads might overlap
            }
        }

        // TyCon: different names → disjoint (nominal)
        (Atom::TyCon(n1), Atom::TyCon(n2)) => n1 != n2,

        // TyCon vs anything else (except App of same TyCon): disjoint
        (Atom::TyCon(_), Atom::Primitive(_)) | (Atom::Primitive(_), Atom::TyCon(_)) => true,
        (Atom::TyCon(_), Atom::Literal(_)) | (Atom::Literal(_), Atom::TyCon(_)) => true,
        (Atom::TyCon(_), Atom::SingleFieldRecord { .. })
        | (Atom::SingleFieldRecord { .. }, Atom::TyCon(_)) => true,
        (Atom::TyCon(_), Atom::Function { .. }) | (Atom::Function { .. }, Atom::TyCon(_)) => true,
        // TyCon vs NominalVariant: disjoint only when the variant is NOT a member of the TyCon.
        (
            Atom::TyCon(n),
            Atom::NominalVariant {
                tycon,
                ctor,
                fields,
            },
        ) => {
            if let Some(env) = tycon_env {
                if let Some(def) = env.get(n.as_str()) {
                    let variant_ty = Type::NominalVariant {
                        tycon: tycon.clone(),
                        ctor: ctor.clone(),
                        fields: fields.clone(),
                    };
                    let mut sigma = HashSet::new();
                    return !Type::is_subtype_bas(&variant_ty, &def.body, tycon_env, &mut sigma);
                }
            }
            tycon != n
        }
        (
            Atom::NominalVariant {
                tycon,
                ctor,
                fields,
            },
            Atom::TyCon(n),
        ) => {
            if let Some(env) = tycon_env {
                if let Some(def) = env.get(n.as_str()) {
                    let variant_ty = Type::NominalVariant {
                        tycon: tycon.clone(),
                        ctor: ctor.clone(),
                        fields: fields.clone(),
                    };
                    let mut sigma = HashSet::new();
                    return !Type::is_subtype_bas(&variant_ty, &def.body, tycon_env, &mut sigma);
                }
            }
            tycon != n
        }

        // Two Function atoms: conservative — different arities or signatures may still share
        // values (e.g., via subtype variance), so we cannot declare them disjoint without a
        // full function-type intersection check. Same function is already caught by reflexivity.
        (Atom::Function { .. }, Atom::Function { .. }) => false,

        // TyCon vs App: a bare type constructor TyCon("Seq") and a fully-applied App
        // (e.g., App(TyCon("Seq"), Int)) are different shapes but not necessarily
        // disjoint (Seq is a constructor, not a type of values). Conservative: false.
        (Atom::TyCon(_), Atom::App(_, _)) | (Atom::App(_, _), Atom::TyCon(_)) => false,
    }
}

// ---------------------------------------------------------------------------
// T-1213: TypeVar bounds
// ---------------------------------------------------------------------------

/// Bounds for a type variable in the BAS constraint solver.
///
/// Each TypeVar alpha has:
/// - lower bounds: types that are subtypes of alpha (alpha must be at least this type)
/// - upper bounds: types that alpha is a subtype of (alpha must be at most this type)
///
/// The effective type of alpha is: Union(lower_bounds) <: alpha <: Intersection(upper_bounds)
///
/// During generalization, bounds are compacted:
/// - If lower = upper = T, alpha is monomorphic, substitute alpha -> T
/// - If lower is empty and upper is [T], alpha <: T, substitute alpha -> T (principal)
/// - If lower is [T] and upper is empty, T <: alpha, substitute alpha -> T (principal)
/// - Otherwise alpha remains polymorphic (free in the TypeScheme)
#[derive(Debug, Clone)]
pub struct TypeVarBounds {
    /// Lower bounds: types L such that L <: alpha
    pub lower: Vec<Type>,
    /// Upper bounds: types U such that alpha <: U
    pub upper: Vec<Type>,
}

impl TypeVarBounds {
    pub fn new() -> Self {
        TypeVarBounds {
            lower: Vec::new(),
            upper: Vec::new(),
        }
    }

    /// Add a lower bound: L <: alpha
    pub fn add_lower(&mut self, ty: Type) {
        if !self.lower.contains(&ty) {
            self.lower.push(ty);
        }
    }

    /// Add an upper bound: alpha <: U
    pub fn add_upper(&mut self, ty: Type) {
        if !self.upper.contains(&ty) {
            self.upper.push(ty);
        }
    }
}

impl Default for TypeVarBounds {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- to_rdnf tests ---

    #[test]
    fn test_rdnf_primitive() {
        let rdnf = to_rdnf(&Type::Int);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 1);
        assert!(matches!(
            &rdnf[0][0],
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))
        ));
    }

    #[test]
    fn test_rdnf_never() {
        let rdnf = to_rdnf(&Type::Never);
        assert!(rdnf.is_empty());
    }

    #[test]
    fn test_rdnf_top() {
        let rdnf = to_rdnf(&Type::Any);
        assert_eq!(rdnf.len(), 1);
        assert!(rdnf[0].is_empty()); // Empty conjunction = Top
    }

    #[test]
    fn test_rdnf_union() {
        // Int | Str → two conjunctions
        let ty = Type::Union(vec![Type::Int, Type::Str]);
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 2);
    }

    #[test]
    fn test_rdnf_intersection() {
        // Int & Str → one conjunction with two atoms (which is empty by disjointness)
        let ty = Type::Intersection(vec![Type::Int, Type::Str]);
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 2);
    }

    #[test]
    fn test_rdnf_negation_primitive() {
        // ~Int → [[Neg(Int)]]
        let ty = Type::Negation(Box::new(Type::Int));
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 1);
        assert!(matches!(
            &rdnf[0][0],
            SignedAtom::Neg(Atom::Primitive(PrimitiveAtom::Int))
        ));
    }

    #[test]
    fn test_rdnf_negation_union() {
        // ~(Int | Str) = ~Int & ~Str → [[Neg(Int), Neg(Str)]]
        let ty = Type::Negation(Box::new(Type::Union(vec![Type::Int, Type::Str])));
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 2);
    }

    #[test]
    fn test_rdnf_negation_intersection() {
        // ~(Int & Str) = ~Int | ~Str → [[Neg(Int)], [Neg(Str)]]
        let ty = Type::Negation(Box::new(Type::Intersection(vec![Type::Int, Type::Str])));
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 2);
        assert_eq!(rdnf[0].len(), 1);
        assert_eq!(rdnf[1].len(), 1);
    }

    #[test]
    fn test_rdnf_double_negation() {
        // ~~Int → [[Pos(Int)]]
        let ty = Type::Negation(Box::new(Type::Negation(Box::new(Type::Int))));
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 1);
        assert!(matches!(
            &rdnf[0][0],
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))
        ));
    }

    #[test]
    fn test_rdnf_multi_field_record() {
        // {x: Int, y: Str} → {x: Int} & {y: Str} → [[Pos({x:Int}), Pos({y:Str})]]
        let ty = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m.insert("y".to_string(), Type::Str);
                m
            },
            tail: RowTail::Empty,
        });
        let rdnf = to_rdnf(&ty);
        assert_eq!(rdnf.len(), 1);
        assert_eq!(rdnf[0].len(), 2);
    }

    // --- is_conjunction_empty tests ---

    #[test]
    fn test_empty_conjunction_is_inhabited() {
        // Empty conjunction = Top = inhabited
        let mut sigma = HashSet::new();
        assert!(!is_conjunction_empty(&vec![], None, &mut sigma));
    }

    #[test]
    fn test_single_positive_is_inhabited() {
        let conj = vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))];
        let mut sigma = HashSet::new();
        assert!(!is_conjunction_empty(&conj, None, &mut sigma));
    }

    #[test]
    fn test_disjoint_positives_is_empty() {
        // Int & Str = Never
        let conj = vec![
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int)),
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Str)),
        ];
        let mut sigma = HashSet::new();
        assert!(is_conjunction_empty(&conj, None, &mut sigma));
    }

    #[test]
    fn test_positive_negated_by_itself_is_empty() {
        // Int & ~Int = Never
        let conj = vec![
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int)),
            SignedAtom::Neg(Atom::Primitive(PrimitiveAtom::Int)),
        ];
        let mut sigma = HashSet::new();
        assert!(is_conjunction_empty(&conj, None, &mut sigma));
    }

    #[test]
    fn test_literal_negated_by_parent_is_empty() {
        // IntLiteral(42) & ~Int = Never (because IntLiteral(42) <: Int)
        let conj = vec![
            SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(42))),
            SignedAtom::Neg(Atom::Primitive(PrimitiveAtom::Int)),
        ];
        let mut sigma = HashSet::new();
        assert!(is_conjunction_empty(&conj, None, &mut sigma));
    }

    #[test]
    fn test_positive_with_unrelated_negation_is_inhabited() {
        // Int & ~Str is inhabited (Int values that aren't Str)
        let conj = vec![
            SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int)),
            SignedAtom::Neg(Atom::Primitive(PrimitiveAtom::Str)),
        ];
        let mut sigma = HashSet::new();
        assert!(!is_conjunction_empty(&conj, None, &mut sigma));
    }

    // --- is_atom_subtype tests ---

    #[test]
    fn test_atom_subtype_reflexive() {
        let atom = Atom::Primitive(PrimitiveAtom::Int);
        let mut sigma = HashSet::new();
        assert!(is_atom_subtype(&atom, &atom, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_int_literal_to_int() {
        let sub = Atom::Literal(LiteralAtom::IntLiteral(42));
        let sup = Atom::Primitive(PrimitiveAtom::Int);
        let mut sigma = HashSet::new();
        assert!(is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_string_literal_to_str() {
        let sub = Atom::Literal(LiteralAtom::StringLiteral("hello".to_string()));
        let sup = Atom::Primitive(PrimitiveAtom::Str);
        let mut sigma = HashSet::new();
        assert!(is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_int_not_subtype_of_str() {
        let sub = Atom::Primitive(PrimitiveAtom::Int);
        let sup = Atom::Primitive(PrimitiveAtom::Str);
        let mut sigma = HashSet::new();
        assert!(!is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_single_field_record_covariant() {
        let sub = Atom::SingleFieldRecord {
            key: "x".to_string(),
            value: Box::new(Type::IntLiteral(42)),
        };
        let sup = Atom::SingleFieldRecord {
            key: "x".to_string(),
            value: Box::new(Type::Int),
        };
        let mut sigma = HashSet::new();
        assert!(is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_single_field_record_different_keys() {
        let sub = Atom::SingleFieldRecord {
            key: "x".to_string(),
            value: Box::new(Type::Int),
        };
        let sup = Atom::SingleFieldRecord {
            key: "y".to_string(),
            value: Box::new(Type::Int),
        };
        let mut sigma = HashSet::new();
        assert!(!is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_nominal_variant_same_tag() {
        let sub = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Ok".to_string(),
            fields: Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("value".to_string(), Type::IntLiteral(42));
                    m
                },
                tail: RowTail::Empty,
            },
        };
        let sup = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Ok".to_string(),
            fields: Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("value".to_string(), Type::Int);
                    m
                },
                tail: RowTail::Empty,
            },
        };
        let mut sigma = HashSet::new();
        assert!(is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    #[test]
    fn test_atom_subtype_nominal_variant_different_tags() {
        let sub = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Ok".to_string(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        let sup = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Err".to_string(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        let mut sigma = HashSet::new();
        assert!(!is_atom_subtype(&sub, &sup, None, 0, &mut sigma));
    }

    // --- atoms_are_disjoint tests ---

    #[test]
    fn test_atoms_disjoint_different_primitives() {
        assert!(atoms_are_disjoint(
            &Atom::Primitive(PrimitiveAtom::Int),
            &Atom::Primitive(PrimitiveAtom::Str),
            None
        ));
    }

    #[test]
    fn test_atoms_not_disjoint_same_primitive() {
        assert!(!atoms_are_disjoint(
            &Atom::Primitive(PrimitiveAtom::Int),
            &Atom::Primitive(PrimitiveAtom::Int),
            None
        ));
    }

    #[test]
    fn test_atoms_disjoint_int_literal_vs_str() {
        assert!(atoms_are_disjoint(
            &Atom::Literal(LiteralAtom::IntLiteral(42)),
            &Atom::Primitive(PrimitiveAtom::Str),
            None
        ));
    }

    #[test]
    fn test_atoms_not_disjoint_int_literal_vs_int() {
        assert!(!atoms_are_disjoint(
            &Atom::Literal(LiteralAtom::IntLiteral(42)),
            &Atom::Primitive(PrimitiveAtom::Int),
            None
        ));
    }

    #[test]
    fn test_atoms_not_disjoint_record_different_keys() {
        // {x:Int} and {y:Int} are NOT disjoint: a value {x:1, y:2} inhabits both.
        // Under BAS open-record semantics, single-field records with different keys
        // form a multi-field record — they are never disjoint. (C-252)
        assert!(!atoms_are_disjoint(
            &Atom::SingleFieldRecord {
                key: "x".to_string(),
                value: Box::new(Type::Int),
            },
            &Atom::SingleFieldRecord {
                key: "y".to_string(),
                value: Box::new(Type::Int),
            },
            None
        ));
    }

    #[test]
    fn test_atoms_disjoint_record_same_key_incompatible_primitives() {
        // {x:Int} and {x:Str} ARE disjoint: no value can have field x typed as both Int and Str.
        // Regression test for SOUND-1: same-key single-field records with incompatible primitive
        // value types must be disjoint, or is_conjunction_empty misses the emptiness of
        // {x:Int} & {x:Str} and incorrectly reports it as inhabited.
        assert!(atoms_are_disjoint(
            &Atom::SingleFieldRecord {
                key: "x".to_string(),
                value: Box::new(Type::Int),
            },
            &Atom::SingleFieldRecord {
                key: "x".to_string(),
                value: Box::new(Type::Str),
            },
            None
        ));
    }

    #[test]
    fn test_atoms_not_disjoint_record_same_key_compatible() {
        // {x:Int} and {x:Int} are NOT disjoint (same type, same value).
        assert!(!atoms_are_disjoint(
            &Atom::SingleFieldRecord {
                key: "x".to_string(),
                value: Box::new(Type::Int),
            },
            &Atom::SingleFieldRecord {
                key: "x".to_string(),
                value: Box::new(Type::Int),
            },
            None
        ));
    }

    // --- is_rdnf_empty integration tests ---

    #[test]
    fn test_rdnf_int_and_not_int_is_empty() {
        // Int & ~Int should be empty
        let ty = Type::Intersection(vec![Type::Int, Type::Negation(Box::new(Type::Int))]);
        let rdnf = to_rdnf(&ty);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_rdnf_int_and_not_str_is_not_empty() {
        // Int & ~Str should NOT be empty (Int values exist that aren't Str)
        let ty = Type::Intersection(vec![Type::Int, Type::Negation(Box::new(Type::Str))]);
        let rdnf = to_rdnf(&ty);
        let mut sigma = HashSet::new();
        assert!(!is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_rdnf_int_and_str_is_empty() {
        // Int & Str should be empty (disjoint primitives)
        let ty = Type::Intersection(vec![Type::Int, Type::Str]);
        let rdnf = to_rdnf(&ty);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_rdnf_int_or_str_and_not_int_is_not_empty() {
        // (Int | Str) & ~Int should NOT be empty (Str values satisfy it)
        let ty = Type::Intersection(vec![
            Type::Union(vec![Type::Int, Type::Str]),
            Type::Negation(Box::new(Type::Int)),
        ]);
        let rdnf = to_rdnf(&ty);
        let mut sigma = HashSet::new();
        assert!(!is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_rdnf_int_literal_and_not_int_is_empty() {
        // IntLiteral(42) & ~Int = Never (IntLiteral(42) <: Int)
        let ty = Type::Intersection(vec![
            Type::IntLiteral(42),
            Type::Negation(Box::new(Type::Int)),
        ]);
        let rdnf = to_rdnf(&ty);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    // --- BAS subtyping integration tests (these test the full A <: B iff A & ~B is empty) ---

    #[test]
    fn test_bas_subtyping_int_subtype_of_int() {
        // Int <: Int iff Int & ~Int is empty → yes
        let sub = Type::Int;
        let sup = Type::Int;
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_int_not_subtype_of_str() {
        // Int <: Str iff Int & ~Str is empty → no
        let sub = Type::Int;
        let sup = Type::Str;
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(!is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_int_literal_subtype_of_int() {
        // IntLiteral(42) <: Int
        let sub = Type::IntLiteral(42);
        let sup = Type::Int;
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_never_subtype_of_anything() {
        // Never <: Int iff Never & ~Int is empty → yes (Never is empty)
        let sub = Type::Never;
        let sup = Type::Int;
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_anything_subtype_of_top() {
        // Int <: Top iff Int & ~Top is empty → yes (~Top = Never)
        let sub = Type::Int;
        let sup = Type::Any;
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_union_subtype_of_supertype() {
        // (Int | Str) <: (Int | Str | Float) iff (Int | Str) & ~(Int | Str | Float) is empty
        let sub = Type::Union(vec![Type::Int, Type::Str]);
        let sup = Type::Union(vec![Type::Int, Type::Str, Type::Float]);
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    #[test]
    fn test_bas_subtyping_record_width_subtyping() {
        // {x: Int, y: Str} <: {x: Int}
        // Under BAS: {x:Int} & {y:Str} & ~{x:Int} = {x:Int} & {y:Str} & ~{x:Int}
        // The {x:Int} and ~{x:Int} cancel → empty
        let sub = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m.insert("y".to_string(), Type::Str);
                m
            },
            tail: RowTail::Empty,
        });
        let sup = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m
            },
            tail: RowTail::Empty,
        });
        let diff = Type::Intersection(vec![sub, Type::Negation(Box::new(sup))]);
        let rdnf = to_rdnf(&diff);
        let mut sigma = HashSet::new();
        assert!(is_rdnf_empty(&rdnf, None, &mut sigma));
    }

    // --- TypeVarBounds tests ---

    #[test]
    fn test_bounds_dedup() {
        let mut bounds = TypeVarBounds::new();
        bounds.add_lower(Type::Int);
        bounds.add_lower(Type::Int); // duplicate
        assert_eq!(bounds.lower.len(), 1);
    }

    /// flatten_rdnf_to_type on an empty RDNF produces Type::Never.
    ///
    /// Empty RDNF = no disjuncts = no way to satisfy the type = Never (the bottom type).
    #[test]
    fn test_flatten_rdnf_empty() {
        let rdnf: Rdnf = vec![];
        let ty = flatten_rdnf_to_type(rdnf);
        assert_eq!(
            ty,
            Type::Never,
            "flatten_rdnf_to_type([]) must produce Never (empty disjunction = bottom)"
        );
    }

    /// flatten_rdnf_to_type on [[]] (single empty conjunction) produces Type::Any.
    ///
    /// An empty conjunction = no constraints = Any (the top type / always satisfied).
    #[test]
    fn test_flatten_rdnf_top() {
        let rdnf: Rdnf = vec![vec![]]; // One conjunction with no atoms = Top
        let ty = flatten_rdnf_to_type(rdnf);
        assert_eq!(
            ty,
            Type::Any,
            "flatten_rdnf_to_type([[]]) must produce Any (empty conjunction = top)"
        );
    }

    // --- B-465: sigma isolation per conjunction in is_rdnf_empty ---

    /// Coinductive assumptions from one conjunction must not leak into another.
    ///
    /// Scenario: RDNF with two conjunctions C1 and C2, where C1 introduces a
    /// coinductive assumption (mu a. A, mu b. B) into sigma during its emptiness check.
    /// C2 must not see that assumption — each disjunct is an independent alternative.
    ///
    /// We construct an RDNF where:
    /// - C1 contains Pos(Recursive{a, Int}) and Neg(Recursive{b, Int}) — this is empty
    ///   because Recursive{a, Int} <: Recursive{b, Int}, but checking it adds (a, b) to sigma.
    /// - C2 contains only Pos(Int) — this is inhabited and should make the RDNF non-empty.
    ///
    /// The key invariant: C2's result must not depend on sigma state from C1.
    #[test]
    fn test_b465_sigma_scoped_per_conjunction() {
        // C1: Pos(mu a. Int) & Neg(mu b. Int) — empty (mu a. Int <: mu b. Int)
        let conj1 = vec![
            SignedAtom::Pos(Atom::Recursive {
                var: "a".to_string(),
                body: Box::new(Type::Int),
            }),
            SignedAtom::Neg(Atom::Recursive {
                var: "b".to_string(),
                body: Box::new(Type::Int),
            }),
        ];
        // C2: Pos(Int) — inhabited
        let conj2 = vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))];

        let rdnf = vec![conj1, conj2];
        let mut sigma = HashSet::new();

        // RDNF = C1 | C2. C2 is inhabited, so the RDNF is NOT empty.
        assert!(
            !is_rdnf_empty(&rdnf, None, &mut sigma),
            "B-465: RDNF with one empty and one inhabited conjunction must not be empty"
        );

        // Verify sigma was not polluted at the call site
        assert!(
            sigma.is_empty(),
            "B-465: caller's sigma must not be mutated by is_rdnf_empty"
        );
    }

    /// Verify that each conjunction gets a fresh sigma by testing that sigma state
    /// from C1 does not cause C2 to wrongly return "empty".
    ///
    /// If sigma leaked, a coinductive assumption (a, b) from C1 could cause C2's
    /// Recursive type check to short-circuit via S-Assum when it should not.
    ///
    /// IMPORTANT: C1 and C2 intentionally use the SAME binder var names "a" and "b".
    /// Under the pre-fix code, C1 inserts ("a","b") into sigma. C2's S-Assum check
    /// then finds ("a","b") already in sigma and short-circuits, incorrectly declaring
    /// C2 empty. Without matching var names, the S-Assum hit would never occur and
    /// the test would not discriminate between the fixed and pre-fix implementations.
    #[test]
    fn test_b465_sigma_leak_would_affect_result() {
        // C1: Pos(mu a. Int) & Neg(mu b. Int) — empty, adds (a, b) to sigma
        let conj1 = vec![
            SignedAtom::Pos(Atom::Recursive {
                var: "a".to_string(),
                body: Box::new(Type::Int),
            }),
            SignedAtom::Neg(Atom::Recursive {
                var: "b".to_string(),
                body: Box::new(Type::Int),
            }),
        ];

        // C2: Pos(mu a. Str) & Neg(mu b. Int) — should NOT be empty (Str ≠ Int bodies),
        // but IF sigma leaked with (a, b) from C1, the S-Assum rule would make
        // is_atom_subtype(Recursive{a, Str}, Recursive{b, Int}) return true incorrectly,
        // making C2 appear empty.
        let conj2 = vec![
            SignedAtom::Pos(Atom::Recursive {
                var: "a".to_string(),
                body: Box::new(Type::Str),
            }),
            SignedAtom::Neg(Atom::Recursive {
                var: "b".to_string(),
                body: Box::new(Type::Int),
            }),
        ];

        let rdnf = vec![conj1, conj2];
        let mut sigma = HashSet::new();

        // C1 is empty (mu a. Int <: mu b. Int). C2 is NOT empty (mu a. Str ≰ mu b. Int).
        // Therefore the RDNF is NOT empty.
        assert!(
            !is_rdnf_empty(&rdnf, None, &mut sigma),
            "B-465: sigma leak from C1 must not cause C2 to appear empty"
        );
    }

    // --- B-467: MAX_RDNF_CONJUNCTIONS limit in distribute() ---

    /// distribute() returns Top RDNF (single empty conjunction) when cross-product would
    /// exceed MAX_RDNF_CONJUNCTIONS. B-590: this is the conservative-safe direction —
    /// Top = inhabited = is_subtype returns false (rejects uncertain subtyping claims).
    #[test]
    fn test_b467_distribute_respects_limit() {
        // Create two RDNFs each with enough conjunctions that their product exceeds 1024.
        // 33 * 33 = 1089 > 1024
        let left: Rdnf = (0..33)
            .map(|i| vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(i)))])
            .collect();
        let right: Rdnf = (100..133)
            .map(|i| vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(i)))])
            .collect();

        let result = distribute(&left, &right);
        // B-590: Must return Top (vec![vec![]]) not Never (vec![]).
        // Top = inhabited = is_subtype returns false = reject when uncertain.
        assert_eq!(
            result,
            vec![vec![]],
            "B-590: distribute() must return Top RDNF when product ({}) exceeds MAX_RDNF_CONJUNCTIONS ({})",
            33 * 33,
            MAX_RDNF_CONJUNCTIONS
        );
    }

    /// distribute() works normally when cross-product is within the limit.
    #[test]
    fn test_b467_distribute_within_limit() {
        // 2 * 3 = 6, well within 1024
        let left: Rdnf = vec![
            vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))],
            vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Str))],
        ];
        let right: Rdnf = vec![
            vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(1)))],
            vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(2)))],
            vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(3)))],
        ];

        let result = distribute(&left, &right);
        assert_eq!(
            result.len(),
            6,
            "B-467: distribute() must produce correct cross-product when within limit"
        );
        // Each result conjunction should have 2 atoms (one from left, one from right)
        for conj in &result {
            assert_eq!(conj.len(), 2);
        }
    }

    /// distribute() at exactly the limit (1024) still works.
    #[test]
    fn test_b467_distribute_at_exact_limit() {
        // 32 * 32 = 1024 = exactly MAX_RDNF_CONJUNCTIONS
        let left: Rdnf = (0..32)
            .map(|i| vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(i)))])
            .collect();
        let right: Rdnf = (100..132)
            .map(|i| vec![SignedAtom::Pos(Atom::Literal(LiteralAtom::IntLiteral(i)))])
            .collect();

        let result = distribute(&left, &right);
        assert_eq!(
            result.len(),
            1024,
            "B-467: distribute() must produce full cross-product at exactly the limit"
        );
    }

    /// distribute() with empty inputs still returns empty (Never & T = Never).
    #[test]
    fn test_b467_distribute_empty_inputs() {
        let empty: Rdnf = vec![];
        let non_empty: Rdnf = vec![vec![SignedAtom::Pos(Atom::Primitive(PrimitiveAtom::Int))]];

        assert!(distribute(&empty, &non_empty).is_empty());
        assert!(distribute(&non_empty, &empty).is_empty());
        assert!(distribute(&empty, &empty).is_empty());
    }

    // --- T-1482: atoms_are_disjoint coverage ---

    /// SingleFieldRecord vs Primitive: always disjoint.
    ///
    /// A record value (e.g., {x: 1}) is never a primitive (e.g., Int). The existing code
    /// at line 987 covers this pair — this test exercises the forward direction.
    #[test]
    fn test_atoms_disjoint_single_field_record_vs_primitive() {
        // {x: Int} and Int are disjoint: no value can be both a record and a primitive.
        assert!(
            atoms_are_disjoint(
                &Atom::SingleFieldRecord {
                    key: "x".to_string(),
                    value: Box::new(Type::Int),
                },
                &Atom::Primitive(PrimitiveAtom::Int),
                None
            ),
            "SingleFieldRecord and Primitive must be disjoint"
        );
        // Commutative: also disjoint in reverse
        assert!(
            atoms_are_disjoint(
                &Atom::Primitive(PrimitiveAtom::Str),
                &Atom::SingleFieldRecord {
                    key: "name".to_string(),
                    value: Box::new(Type::Str),
                },
                None
            ),
            "Primitive and SingleFieldRecord must be disjoint (commutative)"
        );
    }

    /// NominalVariant atoms with different tags are disjoint.
    ///
    /// A value tagged "Color.Red" cannot also be "Color.Blue".
    #[test]
    fn test_atoms_disjoint_nominal_variant_different_tags() {
        let red = Atom::NominalVariant {
            tycon: "Color".to_string(),
            ctor: "Red".to_string(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        let blue = Atom::NominalVariant {
            tycon: "Color".to_string(),
            ctor: "Blue".to_string(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        assert!(
            atoms_are_disjoint(&red, &blue, None),
            "NominalVariant atoms with different tags must be disjoint"
        );
        assert!(
            atoms_are_disjoint(&blue, &red, None),
            "NominalVariant disjointness must be commutative"
        );
    }

    /// NominalVariant atoms with the same tag are NOT disjoint.
    ///
    /// Two atoms with the same tag (e.g., both "Ok") may share values.
    #[test]
    fn test_atoms_not_disjoint_nominal_variant_same_tag() {
        let ok1 = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Ok".to_string(),
            fields: Row {
                fields: IndexMap::new(),
                tail: RowTail::Empty,
            },
        };
        let ok2 = Atom::NominalVariant {
            tycon: "Result".to_string(),
            ctor: "Ok".to_string(),
            fields: Row {
                fields: {
                    let mut m = IndexMap::new();
                    m.insert("value".to_string(), Type::Int);
                    m
                },
                tail: RowTail::Empty,
            },
        };
        assert!(
            !atoms_are_disjoint(&ok1, &ok2, None),
            "NominalVariant atoms with the same tag must NOT be disjoint"
        );
    }

    // --- T-1482: is_atom_subtype contravariant function params ---

    /// Function subtyping is contravariant in parameter types.
    ///
    /// Given: sub = Fn(Int) -> Any, sup = Fn(IntLiteral(42)) -> Any
    /// sub <: sup iff sup_param <: sub_param, i.e., IntLiteral(42) <: Int → true.
    ///
    /// Intuition: if a function accepts any Int, it certainly accepts the specific
    /// literal 42. A caller expecting a function that handles 42 can safely use a
    /// function that handles all Ints.
    #[test]
    fn test_atom_subtype_function_contravariant_param() {
        // sub: Fn(param: Int) -> Any
        let sub = Atom::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Any),
            typed_variadics: vec![],
            rest: None,
            required_count: 1,
        };
        // sup: Fn(param: IntLiteral(42)) -> Any
        let sup = Atom::Function {
            params: vec![(None, Type::IntLiteral(42))],
            ret: Box::new(Type::Any),
            typed_variadics: vec![],
            rest: None,
            required_count: 1,
        };
        let mut sigma = HashSet::new();
        // sub <: sup: a function that accepts Int accepts the specific literal 42.
        // Contravariance check: sup_param (IntLiteral(42)) <: sub_param (Int) → true.
        assert!(
            is_atom_subtype(&sub, &sup, None, 0, &mut sigma),
            "Fn(Int)->Any must be a subtype of Fn(IntLiteral(42))->Any (contravariance)"
        );
    }

    /// Contravariance: the reverse direction does NOT hold.
    ///
    /// Fn(IntLiteral(42)) -> Any is NOT a subtype of Fn(Int) -> Any.
    /// A function that only handles 42 cannot stand in for a function expected to handle
    /// all Ints: contravariance check fails because Int ≰ IntLiteral(42).
    #[test]
    fn test_atom_subtype_function_contravariant_param_reverse_false() {
        // sub: Fn(param: IntLiteral(42)) -> Any
        let sub = Atom::Function {
            params: vec![(None, Type::IntLiteral(42))],
            ret: Box::new(Type::Any),
            typed_variadics: vec![],
            rest: None,
            required_count: 1,
        };
        // sup: Fn(param: Int) -> Any
        let sup = Atom::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Any),
            typed_variadics: vec![],
            rest: None,
            required_count: 1,
        };
        let mut sigma = HashSet::new();
        // sub ≰ sup: a function that only handles 42 cannot substitute for one handling all Ints.
        // Contravariance check: sup_param (Int) <: sub_param (IntLiteral(42)) → false.
        assert!(
            !is_atom_subtype(&sub, &sup, None, 0, &mut sigma),
            "Fn(IntLiteral(42))->Any must NOT be a subtype of Fn(Int)->Any (contravariance)"
        );
    }

    // --- T-1482: to_rdnf for RowTail::Uniform-tailed record ---

    /// A record with a RowTail::Uniform tail converts to a single Atom::Record conjunction.
    ///
    /// Uniform-tailed records (e.g., {_ : Int}) cannot be expressed as a finite intersection
    /// of single-field records — they constrain an infinite set of fields. The to_rdnf rule at
    /// lines 221-222 short-circuits and wraps the entire record as a single Atom::Record atom,
    /// producing [[Pos(Atom::Record(...))]] — a single conjunction with one atom.
    #[test]
    fn test_rdnf_uniform_tailed_record_is_single_atom() {
        let uniform_record = Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m
            },
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Str),
            },
        });
        let rdnf = to_rdnf(&uniform_record);
        // Must produce exactly one conjunction with one atom
        assert_eq!(
            rdnf.len(),
            1,
            "Uniform-tailed record must produce a single-conjunction RDNF"
        );
        assert_eq!(
            rdnf[0].len(),
            1,
            "Uniform-tailed record conjunction must contain exactly one atom"
        );
        // The atom must be Pos(Atom::Record(...)) — not SingleFieldRecord
        assert!(
            matches!(&rdnf[0][0], SignedAtom::Pos(Atom::Record(_))),
            "Uniform-tailed record must become Atom::Record, not Atom::SingleFieldRecord"
        );
    }

    /// An empty record with a Uniform tail also becomes a single Atom::Record.
    ///
    /// Even with no explicit fields, the Uniform tail prevents decomposition.
    #[test]
    fn test_rdnf_empty_fields_uniform_tailed_record_is_single_atom() {
        let empty_uniform = Type::Dict(Row {
            fields: IndexMap::new(),
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Int),
            },
        });
        let rdnf = to_rdnf(&empty_uniform);
        assert_eq!(
            rdnf.len(),
            1,
            "Empty-fields Uniform-tailed record must produce a single-conjunction RDNF"
        );
        assert_eq!(
            rdnf[0].len(),
            1,
            "Empty-fields Uniform-tailed record conjunction must contain exactly one atom"
        );
        assert!(
            matches!(&rdnf[0][0], SignedAtom::Pos(Atom::Record(_))),
            "Empty-fields Uniform-tailed record must become Atom::Record"
        );
    }

    /// End-to-end: deeply nested intersection-of-unions triggers the limit in to_rdnf.
    /// B-590: the result is now Top (inhabited), not Never (uninhabited). This causes
    /// is_subtype to reject uncertain subtyping claims, which is the safe direction.
    #[test]
    fn test_b467_to_rdnf_exponential_intersection() {
        // Build a type: (A0 | B0) & (A1 | B1) & ... & (A10 | B10)
        // This produces 2^11 = 2048 conjunctions, exceeding the limit.
        let members: Vec<Type> = (0..11)
            .map(|i| Type::Union(vec![Type::IntLiteral(i * 2), Type::IntLiteral(i * 2 + 1)]))
            .collect();
        let ty = Type::Intersection(members);
        let rdnf = to_rdnf(&ty);

        // B-590: The RDNF must NOT be empty — overflow returns Top (inhabited = non-empty).
        assert!(
            !rdnf.is_empty(),
            "B-590: to_rdnf on exponential intersection must return non-empty RDNF (Top, not Never)"
        );
    }
}
