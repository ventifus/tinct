//! Exhaustiveness and redundancy checking for pattern matching.
//!
//! Implements the Maranget (2007) usefulness algorithm with the lazy bottom
//! extension from Karachalias et al. (2015). The algorithm operates on an
//! internal `CoveragePattern` representation, separate from the AST `Pattern`.
//!
//! # Formal model
//!
//! The central predicate is **usefulness**: `U(P, q)` — "there exists a value
//! matching `q` that is not matched by any row of matrix `P`."
//!
//! - **Exhaustiveness**: `U(P, [_, _, ..., _])` — is the wildcard vector useful?
//! - **Redundancy**: `¬U(P[1..i-1], P[i])` — is row i useful given prior rows?
//!
//! Two recursive operations decompose the matrix:
//! - `specialize(c, P)` — restrict to rows whose first column matches constructor `c`
//! - `default_matrix(P)` — restrict to rows whose first column is a wildcard
//!
//! # Lazy extension (Maranget 2007, §4; Karachalias et al. 2015, §3.1)
//!
//! Bottom (⊥) is an additional constructor. Wildcards match ⊥; explicit
//! constructors do not. This yields a three-way partition:
//! - **Covered**: value matches and arm RHS fires
//! - **Divergent**: matching forces a ⊥ sub-component, diverges before RHS
//! - **Uncovered**: value definitely doesn't match
//!
//! An arm with Divergent non-empty but Covered empty has an **inaccessible RHS**.
//!
//! # References
//!
//! - Maranget, L. (2007). Warnings for pattern matching. *J. Functional
//!   Programming*, 17(3), 387–421. doi:10.1017/S0956796897002962
//! - Karachalias, G., Schrijvers, T., Vytiniotis, D. & Peyton Jones, S. (2015).
//!   GADTs meet their match. *ICFP '15*, pp. 424–436. doi:10.1145/2784731.2784748

use std::collections::BTreeSet;
use std::fmt;

use crate::ast::{self, LiteralPattern};
use crate::types::{TyConEnv, Type};

// ---------------------------------------------------------------------------
// Coverage pattern representation
// ---------------------------------------------------------------------------

/// Constructor tag — identifies a constructor in the coverage algorithm.
/// Corresponds to the "constructor name" in Maranget's formalization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstructorTag {
    /// Structural variant — a dict with a single discriminant key.
    /// E.g., `[ok: v]` has tag `DictKey("ok")`.
    DictKey(String),
    /// Type tag — matches by runtime type name (e.g., `Int`, `Str`).
    TypeTag(String),
    /// Literal value — matches by exact value.
    LiteralInt(i64),
    LiteralBool(bool),
    LiteralStr(String),
    /// Nominal variant constructor (e.g., `Some`, `None`, `IntLiteral`).
    /// Distinct from `DictKey` — nominal variants use their declared constructor name,
    /// not their field names.
    Variant(String),
    /// Bottom (⊥) — represents a diverging computation.
    /// Wildcards match ⊥; constructors do not.
    Bottom,
}

impl fmt::Display for ConstructorTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstructorTag::DictKey(k) => write!(f, "[{k}: _]"), // Used only in isolation; CoveragePattern::Display handles structured output
            ConstructorTag::TypeTag(t) => write!(f, "{t}"),
            ConstructorTag::LiteralInt(n) => write!(f, "{n}"),
            ConstructorTag::LiteralBool(b) => write!(f, "{b}"),
            ConstructorTag::LiteralStr(s) => write!(f, "\"{s}\""),
            ConstructorTag::Variant(tag) => write!(f, "{tag}"),
            ConstructorTag::Bottom => write!(f, "⊥"),
        }
    }
}

/// Internal pattern representation for coverage analysis.
/// Separate from `ast::Pattern` to decouple the algorithm from AST details.
#[derive(Debug, Clone, PartialEq)]
pub enum CoveragePattern {
    /// Constructor pattern with sub-patterns for each "field" of the constructor.
    Constructor {
        tag: ConstructorTag,
        sub_patterns: Vec<CoveragePattern>,
    },
    /// Wildcard — matches any value (including ⊥).
    Wildcard,
    /// Or-pattern — matches if any alternative matches.
    Or(Vec<CoveragePattern>),
}

impl fmt::Display for CoveragePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoveragePattern::Constructor { tag, sub_patterns } => {
                match tag {
                    ConstructorTag::DictKey(key) => {
                        // Dict constructor: display as [key: payload] syntax
                        if sub_patterns.len() == 1 {
                            write!(f, "[{key}: {}]", sub_patterns[0])
                        } else if sub_patterns.is_empty() {
                            write!(f, "[{key}]")
                        } else {
                            // Multi-field dict — display fields
                            write!(f, "[")?;
                            for (i, (field, sub)) in
                                key.split('\x00').zip(sub_patterns.iter()).enumerate()
                            {
                                if i > 0 {
                                    write!(f, " ")?;
                                }
                                write!(f, "{field}: {sub}")?;
                            }
                            write!(f, "]")
                        }
                    }
                    ConstructorTag::Variant(name) => {
                        // Nominal variant: display as [Tag payload]
                        if sub_patterns.is_empty() {
                            write!(f, "{name}")
                        } else if sub_patterns.len() == 1 {
                            write!(f, "[{name} {}]", sub_patterns[0])
                        } else {
                            write!(f, "[{name}")?;
                            for sub in sub_patterns {
                                write!(f, " {sub}")?;
                            }
                            write!(f, "]")
                        }
                    }
                    _ => {
                        // Type tags, literals, bottom — no sub-patterns expected
                        write!(f, "{tag}")
                    }
                }
            }
            CoveragePattern::Wildcard => write!(f, "_"),
            CoveragePattern::Or(alts) => {
                for (i, alt) in alts.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{alt}")?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor signature — the complete set of constructors for a type
// ---------------------------------------------------------------------------

/// Qualify an unqualified nominal variant tag by looking up its parent TyCon in `tycon_env`.
///
/// `NominalVariant.tag` stores only the unqualified constructor name (e.g., `"Ok"`).
/// Pattern elaboration qualifies constructor tags to `"TyConName.Tag"` (e.g., `"Result.Ok"`).
/// Coverage checking must use the same qualified form so that signature tags and pattern
/// tags are comparable. This function uses `tycon_name` (the HashMap key) to build the
/// qualified form — rather than extracting it from the constructor list — so that within
/// a single matching entry the qualification is deterministic.
///
/// **Limitation:** If two TyCons both declare a constructor with the same unqualified name
/// (e.g., `Result.Ok` and `MyResult.Ok`), `find_map` over a `HashMap` returns one
/// arbitrarily. This is acceptable for now because such name collisions are unusual and
/// coverage is advisory (warnings, not errors).
///
/// Falls back to the unqualified `tag` if no matching TyCon is found (builtin or external type).
fn qualify_nominal_tag(tag: &str, tycon_env: &TyConEnv) -> String {
    tycon_env
        .iter()
        .find_map(|(tycon_name, def)| {
            if def
                .constructors
                .iter()
                .any(|(q, _)| q == &format!("{}.{}", tycon_name, tag))
            {
                Some(format!("{}.{}", tycon_name, tag))
            } else {
                None
            }
        })
        .unwrap_or_else(|| tag.to_string())
}

/// The complete constructor set for a type — used to determine when a match
/// is exhaustive. Maps each constructor tag to its arity (number of sub-patterns).
#[derive(Debug, Clone)]
pub struct ConstructorSignature {
    /// The set of constructors and their arities.
    pub constructors: Vec<(ConstructorTag, usize)>,
}

impl ConstructorSignature {
    /// Build a constructor signature from a `Type::Union`.
    ///
    /// Returns `None` if any union member is a type that cannot be represented
    /// as a finite constructor set (e.g., `Function`, `Unknown`, `TypeVar`, `Top`).
    /// Callers should treat `None` as "skip coverage checking" — not as exhaustive.
    ///
    /// `tycon_env` is used to resolve `Type::TyCon(name)` and `Type::App(TyCon(name), _)`
    /// union members — e.g., `Seq[T]` or a user-defined type constructor. Builtin TyCons
    /// with `builtin_type` set produce a `TypeTag` constructor; user-defined TyCons with
    /// declared constructors produce `Variant` constructors. If `constructors` is empty
    /// (pending population) and `builtin_type` is None, the member is treated as
    /// unrepresentable (skipped, returns `None`).
    pub fn from_union(members: &[Type], tycon_env: &TyConEnv) -> Option<Self> {
        let mut constructors = Vec::new();
        let mut skipped_any = false;
        for member in members {
            match member {
                Type::Record(row) => {
                    // Structural variant — discriminated by the key set.
                    // Each key in the record becomes the constructor tag.
                    // For single-key records (the common case for discriminated unions),
                    // the constructor has arity 1 (the payload).
                    if row.fields.len() == 1 {
                        let (key, _) = row.fields.iter().next().unwrap();
                        constructors.push((ConstructorTag::DictKey(key.clone()), 1));
                    } else {
                        // Multi-key record — use the sorted key set as a combined tag.
                        // Arity = number of fields (each field is a sub-pattern).
                        let mut keys: Vec<&String> = row.fields.keys().collect();
                        keys.sort();
                        let combined = keys
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join("\x00");
                        constructors.push((ConstructorTag::DictKey(combined), row.fields.len()));
                    }
                }
                Type::Int => constructors.push((ConstructorTag::TypeTag("Int".into()), 0)),
                Type::Float => constructors.push((ConstructorTag::TypeTag("Float".into()), 0)),
                Type::Str => constructors.push((ConstructorTag::TypeTag("String".into()), 0)),
                Type::Bool => {
                    // Bool expands to two literal constructors — matches LiteralBool patterns.
                    // TypeTag("Bool") would never match LiteralBool(true/false) patterns.
                    constructors.push((ConstructorTag::LiteralBool(true), 0));
                    constructors.push((ConstructorTag::LiteralBool(false), 0));
                }
                Type::Number => {
                    // Number is not a constructor — it is a supertype of Int and Float.
                    // Expand to the two concrete constructors so they match TypeTag("Int")
                    // and TypeTag("Float") patterns (including the Number Or-pattern expansion
                    // in ast_pattern_to_coverage).
                    constructors.push((ConstructorTag::TypeTag("Int".into()), 0));
                    constructors.push((ConstructorTag::TypeTag("Float".into()), 0));
                }
                Type::StringLiteral(s) => {
                    constructors.push((ConstructorTag::LiteralStr(s.clone()), 0));
                }
                Type::IntLiteral(n) => {
                    constructors.push((ConstructorTag::LiteralInt(*n), 0));
                }
                Type::NominalVariant { tag, fields } => {
                    // Nominal variant — use the declared tag as the constructor name.
                    // Unlike structural dict patterns (which use DictKey with the field name),
                    // nominal variants use the declared variant name as the constructor (Variant tag)
                    // because they are nominally typed — [IntLit value: 42] is not a subtype of
                    // {value: Int}, it is a distinct nominal variant.
                    //
                    // KNOWN ISSUE (Maranget column-consistency): arity is clamped to 0 or 1
                    // rather than fields.len(). This prevents field-by-field recursive
                    // exhaustiveness checking on multi-field variant payloads (e.g.,
                    // [Point x: Int y: Int] with arity 2 would enable nested decomposition).
                    //
                    // Why we can't use fields.len() yet:
                    //   1. Pattern::Constructor has a single `binding`, not per-field bindings.
                    //   2. ast_pattern_to_coverage produces at most 1 sub_pattern (the binding).
                    //   3. The parser only accepts [Constructor payload] with one positional arg.
                    //   Using fields.len() here while the pattern side produces 1 sub-pattern
                    //   would violate Maranget's column-consistency invariant: wildcard rows
                    //   would expand to fields.len() wildcards while constructor rows splice
                    //   only 1 sub-pattern, producing inconsistent matrix row widths.
                    //
                    // To fix properly: extend Pattern::Constructor with per-field bindings,
                    // update the parser to support [Point x y] multi-field patterns, and
                    // update ast_pattern_to_coverage to produce fields.len() sub-patterns.
                    //
                    // B-341: NominalVariant.tag is unqualified (e.g., "Ok" not "Result.Ok").
                    // Look up the parent TyCon in tycon_env to find the qualified constructor name.
                    // If not found, fall back to the unqualified tag (builtin or external type).
                    let qualified_tag = qualify_nominal_tag(tag, tycon_env);
                    let arity = if fields.fields.is_empty() { 0 } else { 1 };
                    constructors.push((ConstructorTag::Variant(qualified_tag), arity));
                }
                // General TyCon / App(TyCon, _) handling: replaces the hardcoded Seq arm.
                // Look up the type constructor name in tycon_env:
                //   - Builtin TyCon (builtin_type is Some, e.g. "Seq", "Map"): emit TypeTag.
                //   - User-defined TyCon with declared constructors: emit Variant for each.
                //   - User-defined TyCon with no constructors (open type or nested dict type, see B-344): skip.
                //   - Unknown TyCon (not in tycon_env): skip (open type, unrepresentable).
                member
                    if matches!(member, Type::TyCon(_))
                        || matches!(member, Type::App(f, _) if matches!(f.as_ref(), Type::TyCon(_))) =>
                {
                    let name = match member {
                        Type::TyCon(n) => n.as_str(),
                        Type::App(f, _) => match f.as_ref() {
                            Type::TyCon(n) => n.as_str(),
                            _ => unreachable!(),
                        },
                        _ => unreachable!(),
                    };
                    match tycon_env.get(name) {
                        Some(def) if !def.constructors.is_empty() => {
                            // User-defined type with known constructors — emit Variant for each.
                            // Arity is clamped to 0/1 (same as NominalVariant) until
                            // Pattern::Constructor supports per-field bindings (T-1003).
                            for (tag, arity) in &def.constructors {
                                let clamped = if *arity == 0 { 0 } else { 1 };
                                constructors.push((ConstructorTag::Variant(tag.clone()), clamped));
                            }
                        }
                        Some(def) if def.builtin_type.is_some() => {
                            // Builtin TyCon (e.g. Seq, Map, Handle) with no declared constructors.
                            // Emit a TypeTag so TypeTag patterns (e.g. [Seq]:) can match it.
                            let tag_name = def.builtin_type.as_deref().unwrap();
                            constructors.push((ConstructorTag::TypeTag(tag_name.to_string()), 0));
                        }
                        Some(_) => {
                            // TyCon found but constructors empty (open type or nested dict type, see B-344)
                            // and no builtin_type — cannot enumerate; treat as unrepresentable.
                            skipped_any = true;
                        }
                        None => {
                            // Unknown TyCon — not in tycon_env, open/external type.
                            skipped_any = true;
                        }
                    }
                }
                _ => {
                    // Type has no finite constructor set (Function, Handle, Unknown, TypeVar,
                    // Top, Error, Intersection, etc.) — cannot verify exhaustiveness statically.
                    skipped_any = true;
                }
            }
        }
        // S-RcdTop regression guard: if two DictKey constructors have the same
        // combined field tag, the union has collapsed to Top (two Record members
        // with identical field sets should unify before reaching coverage).  A
        // duplicate tag means the signature is unsound — coverage would see the
        // same constructor twice and report false exhaustiveness.
        {
            let mut seen_dict_keys: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for (tag, _) in &constructors {
                if let ConstructorTag::DictKey(key) = tag {
                    let count = seen_dict_keys.entry(key.as_str()).or_insert(0);
                    *count += 1;
                    if *count > 1 {
                        panic!(
                            "coverage::ConstructorSignature::from_union: duplicate DictKey \
                             constructor {:?} — union contains two Record members with the same \
                             field set; the type system should have collapsed them to Top before \
                             coverage checking reaches here (S-RcdTop regression)",
                            key
                        );
                    }
                }
            }
        }

        if skipped_any {
            // At least one member was unrepresentable. Returning a partial signature
            // would cause false exhaustiveness: the algorithm would think all
            // constructors are covered when some are invisible to it.
            None
        } else {
            Some(ConstructorSignature { constructors })
        }
    }

    /// Create a signature from a bare NominalVariant (not wrapped in Union).
    /// Extracts a single constructor from the NominalVariant's tag and fields.
    /// Used when coverage checking a match on a bare variant type.
    ///
    /// `tycon_env` is used to qualify the tag (e.g., `"Ok"` → `"Result.Ok"`) so it matches
    /// the elaborated pattern tags produced by `elaborate_pattern`. This mirrors the B-341
    /// fix applied to `from_union`'s NominalVariant arm.
    pub fn from_nominal_variant(
        tag: &str,
        fields: &crate::type_def::Row,
        tycon_env: &TyConEnv,
    ) -> Self {
        // Arity matches the pattern side: 0 for unit variants, 1 for payload variants.
        // See KNOWN ISSUE in from_union for why this is clamped to 0/1 rather than
        // fields.len() — Pattern::Constructor has a single binding, not per-field.
        let arity = if fields.fields.is_empty() { 0 } else { 1 };
        let qualified_tag = qualify_nominal_tag(tag, tycon_env);
        let constructors = vec![(ConstructorTag::Variant(qualified_tag), arity)];
        ConstructorSignature { constructors }
    }

    /// Return the arity of a constructor, or 0 if unknown.
    pub fn arity(&self, tag: &ConstructorTag) -> usize {
        self.constructors
            .iter()
            .find(|(t, _)| t == tag)
            .map(|(_, a)| *a)
            .unwrap_or(0)
    }

    /// The complete set of constructor tags (excluding ⊥).
    pub fn tags(&self) -> BTreeSet<ConstructorTag> {
        self.constructors.iter().map(|(t, _)| t.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// AST Pattern → CoveragePattern conversion
// ---------------------------------------------------------------------------

/// Convert an AST `Pattern` to a `CoveragePattern` for coverage analysis.
///
/// Constructor patterns map to `Constructor { tag: Variant(tag), ... }`.
/// Dict patterns map to `Constructor { tag: DictKey(key), sub_patterns: [payload_pat] }`.
/// TypeAssertPending and TypeAssert patterns map to `Wildcard` (elaboration-pass concern).
/// Wildcards, variables, and literals map to `Wildcard` or typed constructors.
/// Guards are opaque — patterns with guards are treated as wildcards
/// (Karachalias et al. 2015, §2.4).
pub fn ast_pattern_to_coverage(pat: &ast::Pattern) -> CoveragePattern {
    match pat {
        ast::Pattern::Wildcard | ast::Pattern::Pin(_) => CoveragePattern::Wildcard,
        ast::Pattern::TypeTag(tag) => {
            // TypeTag patterns match by runtime type name (Int, Str, Seq, Dict, etc.).
            // Map to CoveragePattern::Constructor with ConstructorTag::TypeTag so the
            // exhaustiveness checker can reason about disjoint type tags.
            CoveragePattern::Constructor {
                tag: ConstructorTag::TypeTag(tag.clone()),
                sub_patterns: vec![],
            }
        }
        ast::Pattern::TypeAssertPending { .. } | ast::Pattern::TypeAssert { .. } => {
            // TypeAssert/TypeAssertPending: treat as wildcard for coverage analysis.
            // Full type-based exhaustiveness is deferred to the elaboration pass.
            CoveragePattern::Wildcard
        }
        ast::Pattern::Literal(lit) => {
            let tag = match lit {
                LiteralPattern::Int(n) => ConstructorTag::LiteralInt(*n),
                LiteralPattern::U64(_) | LiteralPattern::Float(_) => {
                    // U64 and float literals are not suitable for exhaustiveness
                    // (infinite domain) — treat as wildcard
                    return CoveragePattern::Wildcard;
                }
                LiteralPattern::Bool(b) => ConstructorTag::LiteralBool(*b),
                LiteralPattern::Str(s) => ConstructorTag::LiteralStr(s.clone()),
            };
            CoveragePattern::Constructor {
                tag,
                sub_patterns: vec![],
            }
        }
        ast::Pattern::Dict { fields, rest: _ } => {
            if fields.len() == 1 {
                let (key, sub_pat) = &fields[0];
                CoveragePattern::Constructor {
                    tag: ConstructorTag::DictKey(key.clone()),
                    sub_patterns: vec![ast_pattern_to_coverage(&sub_pat.node)],
                }
            } else if fields.is_empty() {
                // Empty dict pattern — treat as wildcard (matches any dict)
                CoveragePattern::Wildcard
            } else {
                // Multi-field dict pattern: use sorted keys as combined tag
                let mut sorted_fields: Vec<_> = fields.iter().collect();
                sorted_fields.sort_by_key(|(k, _)| k.as_str());
                let combined_key = sorted_fields
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join("\x00");
                let sub_pats: Vec<CoveragePattern> = sorted_fields
                    .iter()
                    .map(|(_, p)| ast_pattern_to_coverage(&p.node))
                    .collect();
                CoveragePattern::Constructor {
                    tag: ConstructorTag::DictKey(combined_key),
                    sub_patterns: sub_pats,
                }
            }
        }
        ast::Pattern::Seq { .. } => {
            // Seq patterns are structural — treat as wildcard for now
            // (Seq exhaustiveness requires coinductive reasoning)
            CoveragePattern::Wildcard
        }
        ast::Pattern::Constructor { tag, binding } => {
            let sub_patterns = match binding {
                Some(inner) => vec![ast_pattern_to_coverage(&inner.node)],
                // D-1: [Tag]: with no binding is used for both unit variants (arity 0)
                // and payload variants (arity 1). The sub_patterns vec is left empty here
                // and will be fixed up by normalize_constructor_arities (called from
                // check_coverage) once the sig's declared arity is known.
                // Using vec![] as a sentinel: zero sub-patterns for an unresolved tag.
                None => vec![],
            };
            CoveragePattern::Constructor {
                tag: ConstructorTag::Variant(tag.clone()),
                sub_patterns,
            }
        }
        ast::Pattern::Or(alternatives) => {
            let alts: Vec<CoveragePattern> = alternatives
                .iter()
                .map(|p| ast_pattern_to_coverage(&p.node))
                .collect();
            CoveragePattern::Or(alts)
        }
        // T-1140: Predicate patterns are opaque to coverage analysis (Karachalias et al. 2015, §2.4).
        // Like guards, they depend on runtime values and cannot be statically analyzed.
        ast::Pattern::Predicate(_) => CoveragePattern::Wildcard,
    }
}

/// Normalize `Constructor { tag: Variant(..), sub_patterns }` patterns to match
/// the declared arity in the sig, enforcing Maranget column-consistency.
///
/// Two cases arise from `ast_pattern_to_coverage`:
///
/// 1. `Pattern::Constructor { binding: None }` emits `sub_patterns: vec![]`.
///    - Arity 0 (unit variant): `[Square]:` → keep `vec![]`
///    - Arity 1 (payload variant): `[Circle]:` → upgrade to `vec![Wildcard]`
///
/// 2. `Pattern::Constructor { binding: Some(_) }` emits `sub_patterns: vec![inner]`
///    (arity 1). For a unit variant (sig arity 0), this pattern is dead — it can
///    never match because unit variants have no payload. Dead patterns are represented
///    with a synthetic tag (`__dead_<name>__`) that doesn't appear in the sig and is
///    therefore always dropped by `specialize`, contributing nothing to coverage.
///
/// Maranget (2007) requires that all rows in the pattern matrix have the same width
/// after specialization by any constructor. Arity mismatches produce incorrect row
/// widths and corrupt the algorithm's exhaustiveness/redundancy results.
fn normalize_constructor_arities(
    pat: &CoveragePattern,
    sig: &ConstructorSignature,
) -> CoveragePattern {
    match pat {
        CoveragePattern::Constructor { tag, sub_patterns } => {
            if let ConstructorTag::Variant(name) = tag {
                let sig_arity = sig.arity(tag);
                let pat_arity = sub_patterns.len();

                if pat_arity == 0 {
                    // Bare-tag pattern (binding: None) — normalize to declared arity.
                    let normalized_sub = if sig_arity == 1 {
                        // Payload variant: [Tag]: matches like [Tag _]:
                        vec![CoveragePattern::Wildcard]
                    } else {
                        // Unit variant (arity 0): [Tag]: matches with no sub-patterns
                        vec![]
                    };
                    CoveragePattern::Constructor {
                        tag: tag.clone(),
                        sub_patterns: normalized_sub,
                    }
                } else if pat_arity == 1 && sig_arity == 0 {
                    // Payload-binding pattern ([Tag n]:) for a unit variant — dead pattern.
                    // Unit variants have no payload, so this can never match. Use a synthetic
                    // tag not present in the sig so specialize always drops this row.
                    CoveragePattern::Constructor {
                        tag: ConstructorTag::Variant(format!("__dead_{name}__")),
                        sub_patterns: sub_patterns
                            .iter()
                            .map(|sp| normalize_constructor_arities(sp, sig))
                            .collect(),
                    }
                } else {
                    // Arity already matches sig (pat_arity == sig_arity) — recurse into sub-patterns only.
                    CoveragePattern::Constructor {
                        tag: tag.clone(),
                        sub_patterns: sub_patterns
                            .iter()
                            .map(|sp| normalize_constructor_arities(sp, sig))
                            .collect(),
                    }
                }
            } else {
                // Non-Variant constructor (DictKey, TypeTag, Literal, Bottom) — recurse only.
                CoveragePattern::Constructor {
                    tag: tag.clone(),
                    sub_patterns: sub_patterns
                        .iter()
                        .map(|sp| normalize_constructor_arities(sp, sig))
                        .collect(),
                }
            }
        }
        CoveragePattern::Or(alts) => CoveragePattern::Or(
            alts.iter()
                .map(|a| normalize_constructor_arities(a, sig))
                .collect(),
        ),
        CoveragePattern::Wildcard => CoveragePattern::Wildcard,
    }
}

// ---------------------------------------------------------------------------
// Pattern matrix operations (Maranget 2007, §2)
// ---------------------------------------------------------------------------

/// A pattern vector — one row of the pattern matrix.
pub type PatternVector = Vec<CoveragePattern>;

/// The pattern matrix — each row is a pattern vector.
pub type PatternMatrix = Vec<PatternVector>;

/// Specialize matrix `P` by constructor `c` with arity `a`.
///
/// For each row in `P`:
/// - If first column is `Constructor(c', sub_pats)` where `c' == c`:
///   replace first element with `sub_pats` followed by remaining columns.
/// - If first column is `Wildcard`:
///   replace first element with `a` wildcards followed by remaining columns.
/// - If first column is `Or(alts)`:
///   expand: for each alternative, treat it as the first column and recurse.
/// - Otherwise: drop the row (constructor mismatch).
///
/// Maranget (2007), Definition 2.1.
pub fn specialize(tag: &ConstructorTag, arity: usize, matrix: &PatternMatrix) -> PatternMatrix {
    let mut result = PatternMatrix::new();
    for row in matrix {
        if row.is_empty() {
            continue;
        }
        specialize_row(tag, arity, row, &mut result);
    }
    result
}

fn specialize_row(
    tag: &ConstructorTag,
    arity: usize,
    row: &PatternVector,
    result: &mut PatternMatrix,
) {
    let first = &row[0];
    let rest = &row[1..];
    match first {
        CoveragePattern::Constructor {
            tag: row_tag,
            sub_patterns,
        } => {
            if row_tag == tag {
                // Constructor matches — splice sub_patterns in place of first column
                let mut new_row = sub_patterns.clone();
                new_row.extend_from_slice(rest);
                result.push(new_row);
            }
            // Otherwise: constructor mismatch, row is dropped
        }
        CoveragePattern::Wildcard => {
            // Wildcard matches any constructor — expand to `arity` wildcards
            let mut new_row = vec![CoveragePattern::Wildcard; arity];
            new_row.extend_from_slice(rest);
            result.push(new_row);
        }
        CoveragePattern::Or(alternatives) => {
            // Or-pattern: expand each alternative
            for alt in alternatives {
                let mut expanded_row = vec![alt.clone()];
                expanded_row.extend_from_slice(rest);
                specialize_row(tag, arity, &expanded_row, result);
            }
        }
    }
}

/// Default matrix D(P) — the rows whose first column is a wildcard,
/// with the wildcard removed.
///
/// Maranget (2007), Definition 2.2.
pub fn default_matrix(matrix: &PatternMatrix) -> PatternMatrix {
    let mut result = PatternMatrix::new();
    for row in matrix {
        if row.is_empty() {
            continue;
        }
        default_row(row, &mut result);
    }
    result
}

fn default_row(row: &PatternVector, result: &mut PatternMatrix) {
    let first = &row[0];
    let rest = &row[1..];
    match first {
        CoveragePattern::Wildcard => {
            result.push(rest.to_vec());
        }
        CoveragePattern::Constructor { .. } => {
            // Not a wildcard — drop the row
        }
        CoveragePattern::Or(alternatives) => {
            // Or-pattern: include if any alternative is a wildcard
            for alt in alternatives {
                let mut expanded_row = vec![alt.clone()];
                expanded_row.extend_from_slice(rest);
                default_row(&expanded_row, result);
            }
        }
    }
}

/// Collect the set of constructor tags that appear in the first column
/// of the pattern matrix.
fn head_constructors(matrix: &PatternMatrix) -> BTreeSet<ConstructorTag> {
    let mut tags = BTreeSet::new();
    for row in matrix {
        if row.is_empty() {
            continue;
        }
        collect_head_tags(&row[0], &mut tags);
    }
    tags
}

fn collect_head_tags(pat: &CoveragePattern, tags: &mut BTreeSet<ConstructorTag>) {
    match pat {
        CoveragePattern::Constructor { tag, .. } => {
            tags.insert(tag.clone());
        }
        CoveragePattern::Wildcard => {}
        CoveragePattern::Or(alternatives) => {
            for alt in alternatives {
                collect_head_tags(alt, tags);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Usefulness algorithm (Maranget 2007, §3)
// ---------------------------------------------------------------------------

/// Check whether pattern vector `q` is useful with respect to matrix `P`,
/// given the constructor signature `sig`.
///
/// Returns `true` if there exists a value matching `q` that is NOT matched
/// by any row of `P`.
///
/// This is the strict (non-lazy) usefulness. For lazy semantics with ⊥,
/// use `lazy_useful`.
///
/// Maranget (2007), Algorithm U.
pub fn useful(matrix: &PatternMatrix, q: &PatternVector, sig: &ConstructorSignature) -> bool {
    // Base case: empty pattern vector
    if q.is_empty() {
        // Useful iff the matrix has no rows (no prior pattern matches the empty value)
        return matrix.is_empty();
    }

    let first_q = &q[0];
    let rest_q = &q[1..];

    match first_q {
        CoveragePattern::Constructor { tag, sub_patterns } => {
            // Specialize by this constructor
            let arity = sub_patterns.len();
            let specialized = specialize(tag, arity, matrix);
            let mut new_q = sub_patterns.clone();
            new_q.extend_from_slice(rest_q);
            useful(&specialized, &new_q, sig)
        }
        CoveragePattern::Wildcard => {
            let used_tags = head_constructors(matrix);
            let all_tags = sig.tags();

            if used_tags.is_superset(&all_tags) && !all_tags.is_empty() {
                // Complete signature — check each constructor
                all_tags.iter().any(|tag| {
                    let arity = sig.arity(tag);
                    let specialized = specialize(tag, arity, matrix);
                    let mut new_q = vec![CoveragePattern::Wildcard; arity];
                    new_q.extend_from_slice(rest_q);
                    useful(&specialized, &new_q, sig)
                })
            } else {
                // Incomplete signature — use default matrix
                let def = default_matrix(matrix);
                useful(&def, &rest_q.to_vec(), sig)
            }
        }
        CoveragePattern::Or(alternatives) => {
            // Or-pattern is useful if any alternative is useful
            alternatives.iter().any(|alt| {
                let mut new_q = vec![alt.clone()];
                new_q.extend_from_slice(rest_q);
                useful(matrix, &new_q, sig)
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Lazy usefulness (Maranget 2007, §4)
// ---------------------------------------------------------------------------

/// Lazy usefulness — extends strict usefulness with ⊥ as a constructor.
///
/// A pattern is "lazily useful" if there exists a (possibly partial) value
/// matching the pattern that either:
/// 1. Is not matched by any prior row (strictly useful), or
/// 2. Forces a ⊥ in a prior row, causing divergence before reaching RHS.
///
/// Key insight (Maranget 2007, Theorem 1): lazy usefulness equals strict
/// usefulness when ⊥ is treated as an additional constructor that only
/// wildcards match.
pub fn lazy_useful(matrix: &PatternMatrix, q: &PatternVector, sig: &ConstructorSignature) -> bool {
    // Build an extended signature that includes ⊥
    let mut extended_ctors = sig.constructors.clone();
    extended_ctors.push((ConstructorTag::Bottom, 0));
    let extended_sig = ConstructorSignature {
        constructors: extended_ctors,
    };
    useful(matrix, q, &extended_sig)
}

/// Strict usefulness (without ⊥) — used to distinguish covered from divergent.
/// A pattern is strictly useful if there exists a *total* value (no ⊥ components)
/// matching the pattern that is not matched by prior rows.
pub fn strict_useful(
    matrix: &PatternMatrix,
    q: &PatternVector,
    sig: &ConstructorSignature,
) -> bool {
    useful(matrix, q, sig)
}

// ---------------------------------------------------------------------------
// Coverage analysis result
// ---------------------------------------------------------------------------

/// Result of coverage analysis for a match expression.
#[derive(Debug, Clone)]
pub struct CoverageResult {
    /// Whether the match is exhaustive (all constructors covered).
    pub exhaustive: bool,
    /// Uncovered constructor witnesses — patterns that are not matched.
    pub uncovered: Vec<CoveragePattern>,
    /// Indices of redundant arms (not even lazily useful).
    pub redundant: Vec<usize>,
    /// Indices of arms with inaccessible RHS (lazily useful but not strictly useful —
    /// they are reached only when matching forces ⊥, causing divergence before the RHS).
    pub inaccessible: Vec<usize>,
}

/// Run full coverage analysis on a match expression.
///
/// `arm_patterns` — the patterns from each match arm, converted to `CoveragePattern`.
/// `sig` — the constructor signature from the scrutinee's type.
/// `has_guards` — per-arm flag; arms with guards are opaque to coverage.
///
/// Returns a `CoverageResult` with exhaustiveness, redundancy, and inaccessibility info.
pub fn check_coverage(
    arm_patterns: &[CoveragePattern],
    sig: &ConstructorSignature,
    has_guards: &[bool],
) -> CoverageResult {
    // Normalize bare-tag Constructor patterns so their sub_patterns arity matches the sig.
    // ast_pattern_to_coverage emits vec![] for Pattern::Constructor { binding: None };
    // this pass fixes them to vec![Wildcard] for arity-1 (payload) variants and keeps
    // vec![] for arity-0 (unit) variants. Required for Maranget column-consistency.
    let normalized: Vec<CoveragePattern> = arm_patterns
        .iter()
        .map(|p| normalize_constructor_arities(p, sig))
        .collect();
    let arm_patterns = &normalized;

    let mut matrix: PatternMatrix = Vec::new();
    let mut redundant = Vec::new();
    let mut inaccessible = Vec::new();

    for (i, pat) in arm_patterns.iter().enumerate() {
        let q = vec![pat.clone()];

        if has_guards[i] {
            // Guards are opaque: the arm doesn't contribute to coverage
            // (Karachalias et al. 2015, §2.4). The arm is neither marked
            // redundant nor added to the matrix — it has no effect on
            // coverage analysis for subsequent arms. Adding it as a wildcard
            // would over-approximate coverage and incorrectly mark subsequent
            // arms as redundant.
            continue;
        }

        let is_lazy_useful = lazy_useful(&matrix, &q, sig);
        let is_strict_useful = strict_useful(&matrix, &q, sig);

        if !is_lazy_useful {
            // Not even lazily useful — completely redundant
            redundant.push(i);
        } else if !is_strict_useful {
            // Lazily useful but not strictly useful — inaccessible RHS
            // (reached only via ⊥, which causes divergence before reaching the body)
            inaccessible.push(i);
        }

        matrix.push(q);
    }

    // Check exhaustiveness: is the wildcard vector useful against the full matrix?
    let wildcard_q = vec![CoveragePattern::Wildcard];
    let exhaustive = !strict_useful(&matrix, &wildcard_q, sig);

    // Generate uncovered witnesses
    let uncovered = if exhaustive {
        vec![]
    } else {
        compute_witnesses(sig, &matrix)
    };

    CoverageResult {
        exhaustive,
        uncovered,
        redundant,
        inaccessible,
    }
}

/// Compute witness patterns for uncovered constructors.
///
/// Returns one witness per uncovered constructor — a pattern that matches
/// values not covered by any arm.
fn compute_witnesses(sig: &ConstructorSignature, matrix: &PatternMatrix) -> Vec<CoveragePattern> {
    let mut witnesses = Vec::new();
    for (tag, arity) in &sig.constructors {
        if *tag == ConstructorTag::Bottom {
            continue;
        }
        // Check if this constructor is covered
        let q = vec![CoveragePattern::Constructor {
            tag: tag.clone(),
            sub_patterns: vec![CoveragePattern::Wildcard; *arity],
        }];
        if strict_useful(matrix, &q, sig) {
            // This constructor is not covered — it's a witness
            witnesses.push(CoveragePattern::Constructor {
                tag: tag.clone(),
                sub_patterns: vec![CoveragePattern::Wildcard; *arity],
            });
        }
    }
    witnesses
}

/// Format uncovered witnesses as a human-readable string for error messages.
pub fn format_witnesses(witnesses: &[CoveragePattern]) -> String {
    witnesses
        .iter()
        .map(|w| format!("{w}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Row;

    // Helper: create a constructor pattern
    fn con(tag: ConstructorTag, sub_patterns: Vec<CoveragePattern>) -> CoveragePattern {
        CoveragePattern::Constructor { tag, sub_patterns }
    }

    // Helper: wildcard
    fn wc() -> CoveragePattern {
        CoveragePattern::Wildcard
    }

    // Helper: or-pattern
    fn or(alts: Vec<CoveragePattern>) -> CoveragePattern {
        CoveragePattern::Or(alts)
    }

    // Helper: DictKey constructor (arity 1)
    fn dict_key(key: &str) -> ConstructorTag {
        ConstructorTag::DictKey(key.to_string())
    }

    // Helper: build a sig from tags with arities
    fn sig(tags: &[(ConstructorTag, usize)]) -> ConstructorSignature {
        ConstructorSignature {
            constructors: tags.to_vec(),
        }
    }

    // Helper: Result-like signature: [ok: _] | [err: _]
    fn result_sig() -> ConstructorSignature {
        sig(&[(dict_key("ok"), 1), (dict_key("err"), 1)])
    }

    // Helper: Bool-like signature: true | false
    fn bool_sig() -> ConstructorSignature {
        sig(&[
            (ConstructorTag::LiteralBool(true), 0),
            (ConstructorTag::LiteralBool(false), 0),
        ])
    }

    // ===== Specialize tests =====

    #[test]
    fn test_specialize_constructor_match() {
        // Matrix: [[ok: _], [err: _]]
        // Specialize by "ok" arity 1 → [[_]]
        let matrix = vec![
            vec![con(dict_key("ok"), vec![wc()])],
            vec![con(dict_key("err"), vec![wc()])],
        ];
        let result = specialize(&dict_key("ok"), 1, &matrix);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], vec![wc()]);
    }

    #[test]
    fn test_specialize_wildcard_expands() {
        // Matrix: [[_]]
        // Specialize by "ok" arity 1 → [[_]] (wildcard expands to 1 wildcard)
        let matrix = vec![vec![wc()]];
        let result = specialize(&dict_key("ok"), 1, &matrix);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], vec![wc()]);
    }

    #[test]
    fn test_specialize_or_pattern() {
        // Matrix: [[ok: _ | err: _]]
        // Specialize by "ok" arity 1 → [[_]]
        let matrix = vec![vec![or(vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
        ])]];
        let result = specialize(&dict_key("ok"), 1, &matrix);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], vec![wc()]);
    }

    // ===== Default matrix tests =====

    #[test]
    fn test_default_matrix_keeps_wildcards() {
        // Matrix: [[ok: _], [_]]
        // Default → [[]] (row 0 dropped, row 1 kept with wildcard removed)
        let matrix = vec![vec![con(dict_key("ok"), vec![wc()])], vec![wc()]];
        let result = default_matrix(&matrix);
        assert_eq!(result.len(), 1);
        assert!(result[0].is_empty());
    }

    #[test]
    fn test_default_matrix_drops_constructors() {
        // Matrix: [[ok: _], [err: _]]
        // Default → [] (no wildcards)
        let matrix = vec![
            vec![con(dict_key("ok"), vec![wc()])],
            vec![con(dict_key("err"), vec![wc()])],
        ];
        let result = default_matrix(&matrix);
        assert!(result.is_empty());
    }

    // ===== Usefulness tests =====

    #[test]
    fn test_useful_empty_matrix() {
        // Empty matrix: wildcard is useful (no patterns at all)
        let sig = result_sig();
        assert!(useful(&vec![], &vec![wc()], &sig));
    }

    #[test]
    fn test_useful_complete_coverage() {
        // Matrix: [[ok: _], [err: _]]
        // Wildcard is NOT useful (complete coverage)
        let matrix = vec![
            vec![con(dict_key("ok"), vec![wc()])],
            vec![con(dict_key("err"), vec![wc()])],
        ];
        let sig = result_sig();
        assert!(!useful(&matrix, &vec![wc()], &sig));
    }

    #[test]
    fn test_useful_missing_variant() {
        // Matrix: [[ok: _]]
        // Wildcard IS useful (err not covered)
        let matrix = vec![vec![con(dict_key("ok"), vec![wc()])]];
        let sig = result_sig();
        assert!(useful(&matrix, &vec![wc()], &sig));
    }

    #[test]
    fn test_useful_wildcard_covers_all() {
        // Matrix: [[_]]
        // Wildcard is NOT useful (wildcard covers everything)
        let matrix = vec![vec![wc()]];
        let sig = result_sig();
        assert!(!useful(&matrix, &vec![wc()], &sig));
    }

    #[test]
    fn test_useful_redundant_arm() {
        // Matrix: [[_], [ok: _]]
        // [ok: _] is NOT useful (already covered by wildcard)
        let matrix = vec![vec![wc()]];
        let q = vec![con(dict_key("ok"), vec![wc()])];
        let sig = result_sig();
        assert!(!useful(&matrix, &q, &sig));
    }

    // ===== Coverage result tests =====

    #[test]
    fn test_coverage_complete() {
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        assert!(result.uncovered.is_empty());
        assert!(result.redundant.is_empty());
        assert!(result.inaccessible.is_empty());
    }

    #[test]
    fn test_coverage_missing_variant() {
        let sig = result_sig();
        let patterns = vec![con(dict_key("ok"), vec![wc()])];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(!result.exhaustive);
        assert_eq!(result.uncovered.len(), 1);
        assert_eq!(result.uncovered[0], con(dict_key("err"), vec![wc()]));
    }

    #[test]
    fn test_coverage_redundant_arm() {
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
            con(dict_key("ok"), vec![wc()]), // redundant
        ];
        let guards = vec![false, false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        assert_eq!(result.redundant, vec![2]);
    }

    #[test]
    fn test_coverage_wildcard_exhaustive() {
        let sig = result_sig();
        let patterns = vec![wc()];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        assert!(result.redundant.is_empty());
    }

    #[test]
    fn test_coverage_or_pattern() {
        let sig = result_sig();
        let patterns = vec![or(vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
        ])];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_coverage_guard_opacity() {
        // Guards are opaque: an arm with a guard doesn't contribute to coverage.
        // Here the first arm (ok) has a guard, so it's excluded from the matrix.
        // Only the second arm (err) is in the matrix → ok is uncovered.
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
        ];
        let guards = vec![true, false]; // first arm has a guard
        let result = check_coverage(&patterns, &sig, &guards);
        // NOT exhaustive: guarded arm doesn't contribute, so ok is missing
        assert!(!result.exhaustive);
    }

    #[test]
    fn test_coverage_guard_doesnt_make_redundant() {
        // A guarded arm should not be marked redundant even if the same pattern
        // appears before it without a guard.
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("ok"), vec![wc()]), // same pattern, with guard
            con(dict_key("err"), vec![wc()]),
        ];
        let guards = vec![false, true, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        // Arm 1 has a guard, so it's never marked redundant
        assert!(!result.redundant.contains(&1));
    }

    #[test]
    fn test_coverage_bool_exhaustive() {
        let sig = bool_sig();
        let patterns = vec![
            con(ConstructorTag::LiteralBool(true), vec![]),
            con(ConstructorTag::LiteralBool(false), vec![]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_coverage_bool_missing() {
        let sig = bool_sig();
        let patterns = vec![con(ConstructorTag::LiteralBool(true), vec![])];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(!result.exhaustive);
        assert_eq!(result.uncovered.len(), 1);
    }

    // ===== Lazy bottom extension tests =====

    #[test]
    fn test_lazy_useful_with_bottom() {
        // A constructor pattern is lazily useful if ⊥ can reach it
        // (wildcards match ⊥, constructors don't).
        //
        // Matrix: [[ok: _]]
        // Query: [_] (wildcard)
        // Strict useful: yes (err not covered)
        // Lazy useful: yes (⊥ also not covered)
        let matrix = vec![vec![con(dict_key("ok"), vec![wc()])]];
        let q = vec![wc()];
        let sig = result_sig();
        assert!(strict_useful(&matrix, &q, &sig));
        assert!(lazy_useful(&matrix, &q, &sig));
    }

    #[test]
    fn test_inaccessible_rhs() {
        // An arm that is lazily useful but not strictly useful has an inaccessible RHS.
        //
        // Signature: ok | err
        // Arms: [ok: _], [err: _], [_]
        //
        // Arm 2 (wildcard) is lazily useful (⊥ matches wildcard but not constructors)
        // but not strictly useful (all total values already covered).
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
            wc(),
        ];
        let guards = vec![false, false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        assert!(result.redundant.is_empty());
        assert_eq!(result.inaccessible, vec![2]);
    }

    #[test]
    fn test_inaccessible_rhs_with_nested_bottom() {
        // Nested pattern: [ok: Int] where ok payload could be ⊥
        //
        // Sig for payload: {Int, Str} (inner type)
        // But for the outer level we just need ok|err
        //
        // At the outer level: [ok: _], [err: _], [_]
        // The wildcard catches ⊥ at the outer level → inaccessible RHS
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("err"), vec![wc()]),
            wc(),
        ];
        let guards = vec![false, false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert_eq!(result.inaccessible, vec![2]);
    }

    // ===== Nested pattern tests =====

    #[test]
    fn test_nested_dict_pattern_exhaustive() {
        // [ok: [some: _]] | [ok: [none]] | [err: _]
        // This tests nested pattern decomposition.
        //
        // For the outer level we have a Result-like sig: ok | err.
        // For the inner level of ok's payload, we'd need an Option-like sig.
        //
        // With just the outer sig, this is complete since ok and err are covered.
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]), // ok with wildcard payload
            con(dict_key("err"), vec![wc()]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_nested_pattern_redundancy() {
        // [ok: _], [ok: _] — second ok is redundant even with nesting
        let sig = result_sig();
        let patterns = vec![
            con(dict_key("ok"), vec![wc()]),
            con(dict_key("ok"), vec![wc()]), // redundant
            con(dict_key("err"), vec![wc()]),
        ];
        let guards = vec![false, false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
        assert_eq!(result.redundant, vec![1]);
    }

    // ===== Nominal variant tests =====

    #[test]
    fn test_nominal_variant_coverage() {
        // Maybe: Maybe.Some(_) | Maybe.None
        let sig = sig(&[
            (ConstructorTag::Variant("Maybe.Some".into()), 1),
            (ConstructorTag::Variant("Maybe.None".into()), 0),
        ]);
        let patterns = vec![
            con(ConstructorTag::Variant("Maybe.Some".into()), vec![wc()]),
            con(ConstructorTag::Variant("Maybe.None".into()), vec![]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_nominal_variant_missing() {
        let sig = sig(&[
            (ConstructorTag::Variant("Maybe.Some".into()), 1),
            (ConstructorTag::Variant("Maybe.None".into()), 0),
        ]);
        let patterns = vec![con(
            ConstructorTag::Variant("Maybe.Some".into()),
            vec![wc()],
        )];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(!result.exhaustive);
        assert_eq!(result.uncovered.len(), 1);
        assert_eq!(
            result.uncovered[0],
            con(ConstructorTag::Variant("Maybe.None".into()), vec![])
        );
    }

    // ===== Type tag coverage tests =====

    #[test]
    fn test_type_tag_int_str_coverage() {
        let sig = sig(&[
            (ConstructorTag::TypeTag("Int".into()), 0),
            (ConstructorTag::TypeTag("String".into()), 0),
        ]);
        let patterns = vec![
            con(ConstructorTag::TypeTag("Int".into()), vec![]),
            con(ConstructorTag::TypeTag("String".into()), vec![]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_type_tag_missing() {
        let sig = sig(&[
            (ConstructorTag::TypeTag("Int".into()), 0),
            (ConstructorTag::TypeTag("String".into()), 0),
        ]);
        let patterns = vec![con(ConstructorTag::TypeTag("Int".into()), vec![])];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(!result.exhaustive);
        assert_eq!(result.uncovered.len(), 1);
    }

    // ===== AST pattern conversion tests =====

    #[test]
    fn test_ast_wildcard_to_coverage() {
        let pat = ast::Pattern::Wildcard;
        let coverage = ast_pattern_to_coverage(&pat);
        assert_eq!(coverage, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_pin_to_coverage() {
        // T-1154: Pattern::Variable retired; bare names are now Pattern::Pin.
        // Pin patterns map to Wildcard for coverage purposes (same as before).
        let pat = ast::Pattern::Pin("x".to_string());
        let coverage = ast_pattern_to_coverage(&pat);
        assert_eq!(coverage, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_type_assert_pending_to_coverage() {
        use crate::ast::{Annotation, Position, Span, Spanned};
        let span = Span {
            start: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            end: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            file: None,
        };
        // TypeAssertPending is treated as Wildcard for coverage analysis.
        let pat = ast::Pattern::TypeAssertPending {
            annotation: Spanned {
                node: Annotation::Simple("Int".to_string()),
                span,
            },
            inner: None,
        };
        let coverage = ast_pattern_to_coverage(&pat);
        assert_eq!(coverage, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_dict_to_coverage() {
        use crate::ast::{Position, Span, Spanned};
        let span = Span {
            start: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            end: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            file: None,
        };
        let pat = ast::Pattern::Dict {
            fields: vec![(
                "ok".to_string(),
                Spanned {
                    node: ast::Pattern::Pin("v".to_string()),
                    span,
                },
            )],
            rest: true,
        };
        let coverage = ast_pattern_to_coverage(&pat);
        assert_eq!(
            coverage,
            CoveragePattern::Constructor {
                tag: ConstructorTag::DictKey("ok".to_string()),
                sub_patterns: vec![CoveragePattern::Wildcard], // variable → wildcard
            }
        );
    }

    #[test]
    fn test_ast_constructor_to_coverage() {
        use crate::ast::{Position, Span, Spanned};
        let span = Span {
            start: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            end: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            file: None,
        };
        let pat = ast::Pattern::Constructor {
            tag: "Maybe.Some".to_string(),
            binding: Some(Box::new(Spanned {
                node: ast::Pattern::Pin("x".to_string()),
                span,
            })),
        };
        let coverage = ast_pattern_to_coverage(&pat);
        assert_eq!(
            coverage,
            CoveragePattern::Constructor {
                tag: ConstructorTag::Variant("Maybe.Some".to_string()),
                sub_patterns: vec![CoveragePattern::Wildcard],
            }
        );
    }

    #[test]
    fn test_ast_or_pattern_to_coverage() {
        use crate::ast::{Position, Span, Spanned};
        let span = Span {
            start: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            end: Position {
                line: 0,
                column: 0,
                offset: 0,
            },
            file: None,
        };
        let pat = ast::Pattern::Or(vec![
            Spanned {
                node: ast::Pattern::Constructor {
                    tag: "Int".to_string(),
                    binding: None,
                },
                span: span.clone(),
            },
            Spanned {
                node: ast::Pattern::Constructor {
                    tag: "Float".to_string(),
                    binding: None,
                },
                span,
            },
        ]);
        let coverage = ast_pattern_to_coverage(&pat);
        match &coverage {
            CoveragePattern::Or(alts) => {
                assert_eq!(alts.len(), 2);
            }
            other => panic!("expected Or, got {other}"),
        }
    }

    // ===== Constructor signature from Type::Union tests =====

    #[test]
    fn test_sig_from_union_record_variants() {
        let union_members = vec![
            Type::Record(Row {
                fields: [("ok".to_string(), Type::Unknown)].into_iter().collect(),
                tail: crate::type_def::RowTail::Empty,
            }),
            Type::Record(Row {
                fields: [("err".to_string(), Type::Str)].into_iter().collect(),
                tail: crate::type_def::RowTail::Empty,
            }),
        ];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("all members are representable");
        assert_eq!(sig.constructors.len(), 2);
        let tags = sig.tags();
        assert!(tags.contains(&ConstructorTag::DictKey("ok".to_string())));
        assert!(tags.contains(&ConstructorTag::DictKey("err".to_string())));
    }

    #[test]
    fn test_sig_from_union_primitive_types() {
        let union_members = vec![Type::Int, Type::Str];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("all members are representable");
        assert_eq!(sig.constructors.len(), 2);
        let tags = sig.tags();
        assert!(tags.contains(&ConstructorTag::TypeTag("Int".to_string())));
        assert!(tags.contains(&ConstructorTag::TypeTag("String".to_string())));
    }

    #[test]
    fn test_sig_from_union_string_literals() {
        let union_members = vec![
            Type::StringLiteral("ok".to_string()),
            Type::StringLiteral("err".to_string()),
        ];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("all members are representable");
        assert_eq!(sig.constructors.len(), 2);
    }

    #[test]
    fn test_sig_from_union_with_unrepresentable_type_returns_none() {
        // Function types cannot be expressed as a finite constructor set.
        // from_union must return None rather than a partial (unsound) signature.
        let union_members = vec![
            Type::Int,
            Type::Function {
                params: vec![],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        ];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new());
        assert!(
            sig.is_none(),
            "union containing Function must return None — cannot verify exhaustiveness"
        );
    }

    #[test]
    fn test_sig_from_union_bool_expands_to_literal_bool() {
        // Type::Bool must expand to LiteralBool(true) and LiteralBool(false),
        // not TypeTag("Bool"), so it matches LiteralBool patterns.
        let union_members = vec![Type::Bool];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("Bool is representable");
        let tags = sig.tags();
        assert!(
            tags.contains(&ConstructorTag::LiteralBool(true)),
            "Bool must produce LiteralBool(true)"
        );
        assert!(
            tags.contains(&ConstructorTag::LiteralBool(false)),
            "Bool must produce LiteralBool(false)"
        );
        assert!(
            !tags.contains(&ConstructorTag::TypeTag("Bool".to_string())),
            "Bool must NOT produce TypeTag(\"Bool\") — patterns use LiteralBool"
        );
    }

    #[test]
    fn test_sig_from_union_number_expands_to_int_and_float() {
        // Type::Number must expand to TypeTag("Int") and TypeTag("Float"),
        // not TypeTag("Number"), so it matches Number or-pattern expansion.
        let union_members = vec![Type::Number];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("Number is representable");
        let tags = sig.tags();
        assert!(
            tags.contains(&ConstructorTag::TypeTag("Int".to_string())),
            "Number must produce TypeTag(\"Int\")"
        );
        assert!(
            tags.contains(&ConstructorTag::TypeTag("Float".to_string())),
            "Number must produce TypeTag(\"Float\")"
        );
        assert!(
            !tags.contains(&ConstructorTag::TypeTag("Number".to_string())),
            "Number must NOT produce TypeTag(\"Number\") — no such constructor"
        );
    }

    // ===== Witness formatting tests =====

    #[test]
    fn test_format_witnesses_dict() {
        let witnesses = vec![con(dict_key("err"), vec![wc()])];
        let formatted = format_witnesses(&witnesses);
        assert_eq!(formatted, "[err: _]");
    }

    #[test]
    fn test_format_witnesses_multiple() {
        let witnesses = vec![
            con(dict_key("err"), vec![wc()]),
            con(dict_key("warn"), vec![wc()]),
        ];
        let formatted = format_witnesses(&witnesses);
        assert!(formatted.contains("[err: _]"));
        assert!(formatted.contains("[warn: _]"));
    }

    // ===== normalize_constructor_arities tests =====

    // Sig helper: Maybe-like Union (Maybe.Some=arity-1, Maybe.None=arity-0)
    fn option_variant_sig() -> ConstructorSignature {
        sig(&[
            (ConstructorTag::Variant("Maybe.Some".into()), 1),
            (ConstructorTag::Variant("Maybe.None".into()), 0),
        ])
    }

    #[test]
    fn test_normalize_bare_tag_payload_variant_emits_wildcard() {
        // binding:None on a payload variant (arity 1): bare [Maybe.Some]: with sub_patterns=[]
        // normalize_constructor_arities must expand it to sub_patterns=[Wildcard],
        // matching like [Maybe.Some _]:
        let sig = option_variant_sig();
        let bare_some = con(ConstructorTag::Variant("Maybe.Some".into()), vec![]);
        let normalized = normalize_constructor_arities(&bare_some, &sig);
        assert_eq!(
            normalized,
            con(ConstructorTag::Variant("Maybe.Some".into()), vec![wc()]),
            "bare-tag [Maybe.Some]: on arity-1 variant must normalize to [Maybe.Some _]:"
        );
    }

    #[test]
    fn test_normalize_bare_tag_unit_variant_emits_empty() {
        // binding:None on a unit variant (arity 0): bare [Maybe.None]: with sub_patterns=[]
        // normalize_constructor_arities must keep sub_patterns=[] (unit — no payload slot)
        let sig = option_variant_sig();
        let bare_none = con(ConstructorTag::Variant("Maybe.None".into()), vec![]);
        let normalized = normalize_constructor_arities(&bare_none, &sig);
        assert_eq!(
            normalized,
            con(ConstructorTag::Variant("Maybe.None".into()), vec![]),
            "bare-tag [Maybe.None]: on arity-0 variant must normalize to [] sub-patterns"
        );
    }

    #[test]
    fn test_normalize_binding_some_on_unit_variant_emits_dead_pattern() {
        // binding:Some on a unit variant (arity 0): [Maybe.None n]: with sub_patterns=[Wildcard]
        // normalize_constructor_arities must convert to a __dead_Maybe.None__ tag so specialize
        // always drops this row — the pattern can never match (unit variants have no payload).
        let sig = option_variant_sig();
        let payload_none = con(ConstructorTag::Variant("Maybe.None".into()), vec![wc()]);
        let normalized = normalize_constructor_arities(&payload_none, &sig);
        match &normalized {
            CoveragePattern::Constructor { tag, .. } => {
                assert_eq!(
                    *tag,
                    ConstructorTag::Variant("__dead_Maybe.None__".into()),
                    "binding on unit variant must produce a __dead__ synthetic tag"
                );
            }
            other => panic!("expected Constructor, got {other}"),
        }
    }
}
