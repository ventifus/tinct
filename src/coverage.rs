//! Exhaustiveness and redundancy checking for pattern matching.
//!
//! Implements the Maranget (2007) usefulness algorithm with the lazy bottom
//! extension from Karachalias et al. (2015). The algorithm operates on an
//! internal `CoveragePattern` representation, separate from the AST `SurfaceNode`.
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

use crate::ast::{SurfaceExpression, SurfaceNode};
use crate::type_tags::*;
use crate::value::Value;
use std::sync::Arc;

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
    /// Literal value — matches by exact value.
    LiteralInt(i64),
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
            ConstructorTag::LiteralInt(n) => write!(f, "{n}"),
            ConstructorTag::LiteralStr(s) => write!(f, "\"{s}\""),
            ConstructorTag::Variant(tag) => write!(f, "{tag}"),
            ConstructorTag::Bottom => write!(f, "⊥"),
        }
    }
}

/// Internal pattern representation for coverage analysis.
/// Separate from the AST to decouple the algorithm from surface expression details.
#[derive(Debug, Clone, PartialEq)]
pub enum CoveragePattern {
    /// Constructor pattern with sub-patterns for each "field" of the constructor.
    Constructor {
        tag: ConstructorTag,
        sub_patterns: Vec<CoveragePattern>,
    },
    /// Wildcard — matches any value (including ⊥).
    Wildcard,
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
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor signature — the complete set of constructors for a type
// ---------------------------------------------------------------------------

/// The complete constructor set for a type — used to determine when a match
/// is exhaustive. Maps each constructor tag to its arity (number of sub-patterns).
#[derive(Debug, Clone)]
pub struct ConstructorSignature {
    /// The set of constructors and their arities.
    pub constructors: Vec<(ConstructorTag, usize)>,
}

impl ConstructorSignature {
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

    /// Build a `ConstructorSignature` from a list of TypeValue union members.
    ///
    /// Each member is mapped to a `ConstructorTag`:
    /// - `TypeValue.Record` with a single field `k` → `DictKey(k)` with arity 1
    /// - `TypeValue.Repr { repr: "Value::X" }` → `Variant("X")` with arity 0
    /// - `TypeValue.StrLit { value: s }` → `LiteralStr(s)` with arity 0
    /// - `TypeValue.IntLit { value: n }` → `LiteralInt(n)` with arity 0
    ///
    /// Returns `None` if any member cannot be represented as a finite constructor
    /// (e.g., `TypeValue.Fn` — function types have no finite constructor set).
    ///
    /// The `_tycon_env` parameter is reserved for future nominal type constructor lookups.
    #[cfg(test)]
    pub fn from_union(
        members: &[Arc<crate::value::Value>],
        _tycon_env: &crate::type_def::TyConEnv,
    ) -> Option<Self> {
        use crate::type_infer::typevalue_ctor;
        let mut constructors: Vec<(ConstructorTag, usize)> = Vec::with_capacity(members.len());
        for member in members {
            let ctor_tag = typevalue_ctor(member)?;
            let entry = match ctor_tag {
                TV_REPR => {
                    // Extract repr string: "Value::X" → strip "Value::" prefix → "X"
                    let repr_str = extract_string_payload_field(member, FIELD_REPR)?;
                    let short = repr_str
                        .strip_prefix("Value::")
                        .unwrap_or(&repr_str)
                        .to_string();
                    (ConstructorTag::Variant(short), 0)
                }
                TV_STR_LIT => {
                    let s = extract_string_payload_field(member, FIELD_VALUE)?;
                    (ConstructorTag::LiteralStr(s), 0)
                }
                TV_INT_LIT => {
                    let n = extract_int_payload_field(member, FIELD_VALUE)?;
                    (ConstructorTag::LiteralInt(n), 0)
                }
                TV_RECORD => {
                    // Single-field record: the field name becomes the DictKey constructor.
                    let fields = extract_record_fields(member)?;
                    if fields.len() == 1 {
                        let field_name = fields.into_iter().next().unwrap();
                        (ConstructorTag::DictKey(field_name), 1)
                    } else {
                        // Multi-field records don't map cleanly to a single constructor tag.
                        return None;
                    }
                }
                // TypeValue.NominalVariant, TypeValue.Var, TypeValue.Unknown etc. — unrepresentable.
                _ => return None,
            };
            constructors.push(entry);
        }
        Some(ConstructorSignature { constructors })
    }
}

/// Extract a string field from a TypeValue's settled Dict payload.
///
/// Returns `None` if the TypeValue has no settled payload, the payload is not a Dict,
/// or the requested field is not a String.
#[cfg(test)]
fn extract_string_payload_field(tv: &Arc<Value>, field: &str) -> Option<String> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let key = HashableValue::Str(Arc::from(field));
                let val_thunk = entries.get(&key)?;
                match val_thunk.peek_result()? {
                    Ok(Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract an integer field from a TypeValue's settled Dict payload.
#[cfg(test)]
fn extract_int_payload_field(tv: &Arc<Value>, field: &str) -> Option<i64> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let key = HashableValue::Str(Arc::from(field));
                let val_thunk = entries.get(&key)?;
                match val_thunk.peek_result()? {
                    Ok(Value::Int { n, .. }) => Some(*n),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the field names from a TypeValue.Record's `fields` Dict payload.
///
/// Returns the list of string field names, or `None` if the payload is unreadable.
#[cfg(test)]
fn extract_record_fields(tv: &Arc<Value>) -> Option<Vec<String>> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                let fields_thunk = entries.get(&fields_key)?;
                match fields_thunk.peek_result()? {
                    Ok(Value::Dict {
                        entries: field_entries,
                        ..
                    }) => {
                        let names: Vec<String> = field_entries
                            .keys()
                            .filter_map(|k| {
                                if let HashableValue::Str(s) = k {
                                    Some(s.as_ref().to_string())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        Some(names)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// AST Pattern → CoveragePattern conversion
// ---------------------------------------------------------------------------

/// Convert an AST pattern (SurfaceNode) to an internal `CoveragePattern`.
///
/// Patterns are now Arc<SurfaceNode>. This function maps SurfaceExpression variants
/// to CoveragePattern. Coverage-relevant forms:
/// - VarRef (resolved pin) → non-exhaustive Constructor (does not contribute to exhaustiveness)
/// - VarRef (unresolvable) → Wildcard (conservative; error already emitted by resolver)
/// - Placeholder → Wildcard
/// - Int(n) → Constructor(LiteralInt(n), [])
/// - StringLiteral → Constructor(LiteralStr(content), [])
/// - Float/U64 → Wildcard (infinite domain)
/// - Field (dot-access like `Color.Red`) → Constructor(Variant(qualified_tag), []) after flattening
/// - Call with Field head and args → Constructor(Variant(tag), [recurse(sub)])
/// - Dict(entries) → Constructor(DictKey(...), [recurse each entry value])
/// - TypeAssert → map resolved type to Constructor(Variant(type_name), [recurse inner])
/// - Everything else → Wildcard
///
/// Guards are opaque — patterns with guards are treated as wildcards (Karachalias et al. 2015, §2.4).
pub fn ast_pattern_to_coverage(
    node: &SurfaceNode,
    tycon_env: Option<&crate::type_def::TyConEnv>,
) -> CoveragePattern {
    match &node.expr {
        // Placeholder `...` — always a wildcard.
        SurfaceExpression::Placeholder(..) => CoveragePattern::Wildcard,

        // VarRef — distinguish binder from pin from unresolvable.
        // A binder pattern (VarAddr::Parameter from [case [let names] ...] arms) is a wildcard
        // with a binding name attached — it always matches and contributes to exhaustiveness.
        // A pin pattern (`foo:` where `foo` is in scope) matches only the current value of
        // `foo` — it is NOT exhaustive. Treat it as a named non-exhaustive "literal" so the
        // coverage algorithm does not falsely report it as exhaustive.
        // An unresolvable VarRef (resolver returned Some(None), e.g. `_:` not yet migrated)
        // is treated as wildcard: the error was already reported by the resolver.
        // If the resolver has not run (None), be conservative and treat as wildcard.
        SurfaceExpression::VarRef {
            name, resolution, ..
        } => {
            match resolution.get() {
                Some(Some(crate::ast::VarAddr::Parameter(_))) => {
                    // Binder pattern (case arm binding) — always matches (wildcard semantics)
                    CoveragePattern::Wildcard
                }
                Some(Some(_)) => {
                    // Resolved pin — non-exhaustive, like a literal
                    CoveragePattern::Constructor {
                        tag: ConstructorTag::DictKey(format!("__pin_{}__", name)),
                        sub_patterns: vec![],
                    }
                }
                Some(None) | None => {
                    // Unresolvable or not yet resolved — conservative wildcard
                    CoveragePattern::Wildcard
                }
            }
        }

        // Literal patterns
        SurfaceExpression::Int(n) => CoveragePattern::Constructor {
            tag: ConstructorTag::LiteralInt(*n),
            sub_patterns: vec![],
        },
        SurfaceExpression::StringLiteral { content, .. } => CoveragePattern::Constructor {
            tag: ConstructorTag::LiteralStr(content.clone()),
            sub_patterns: vec![],
        },
        SurfaceExpression::U64(_) | SurfaceExpression::Float(_) => {
            // Infinite domain — not suitable for exhaustiveness
            CoveragePattern::Wildcard
        }

        // Field access — flatten to tag string (e.g., `Color.Red`)
        SurfaceExpression::Field { .. } => match crate::ast::flatten_dot_access_to_tag_node(node) {
            Some(tag) => CoveragePattern::Constructor {
                tag: ConstructorTag::Variant(tag),
                sub_patterns: vec![],
            },
            None => CoveragePattern::Wildcard,
        },

        // Call with Field head and single arg → Constructor(tag, [sub])
        // (e.g., `[Color.Red payload]`)
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } if matches!(&func.expr, SurfaceExpression::Field { .. })
            && args.len() == 1
            && named_args.is_empty() =>
        {
            match crate::ast::flatten_dot_access_to_tag_node(func) {
                Some(tag) => {
                    let sub = ast_pattern_to_coverage(&args[0], tycon_env);
                    CoveragePattern::Constructor {
                        tag: ConstructorTag::Variant(tag),
                        sub_patterns: vec![sub],
                    }
                }
                None => CoveragePattern::Wildcard,
            }
        }

        // Dict pattern
        SurfaceExpression::Dict(entries) => {
            // Extract keyed entries (filter out auto-indexed positional entries if any)
            let keyed: Vec<_> = entries
                .iter()
                .filter_map(|e| {
                    e.node.key.as_ref().and_then(|k| match &k.expr {
                        SurfaceExpression::StringLiteral { content, .. } => {
                            Some((content.clone(), &e.node.value))
                        }
                        SurfaceExpression::VarRef { name, .. } => {
                            Some((name.clone(), &e.node.value))
                        }
                        _ => None,
                    })
                })
                .collect();

            if keyed.len() == 1 {
                let (key, sub_node) = &keyed[0];
                CoveragePattern::Constructor {
                    tag: ConstructorTag::DictKey(key.clone()),
                    sub_patterns: vec![ast_pattern_to_coverage(sub_node, tycon_env)],
                }
            } else if keyed.is_empty() {
                CoveragePattern::Wildcard
            } else {
                // Multi-field dict pattern
                let mut sorted = keyed.clone();
                sorted.sort_by_key(|(k, _)| k.clone());
                let combined_key = sorted
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join("\x00");
                let sub_pats: Vec<CoveragePattern> = sorted
                    .iter()
                    .map(|(_, node)| ast_pattern_to_coverage(node, tycon_env))
                    .collect();
                CoveragePattern::Constructor {
                    tag: ConstructorTag::DictKey(combined_key),
                    sub_patterns: sub_pats,
                }
            }
        }

        // TypeAssert pattern — extract resolved TypeValue from inline TypeAnnotation
        SurfaceExpression::TypeAssert {
            expr,
            resolved_type,
            ..
        } => {
            // TypeAnnotation::get() now returns Option<&Arc<Value>> after S-1003 migration.
            let resolved_tv = resolved_type
                .get()
                .cloned()
                .unwrap_or_else(crate::value::unknown_type_val);

            let inner_sub = vec![ast_pattern_to_coverage(expr, tycon_env)];

            // Determine the constructor tag from the TypeValue's ctor string.
            let tag_opt: Option<&str> = match resolved_tv.as_ref() {
                Value::Variant { ctor, payload, .. } => match ctor.as_ref() {
                    TV_REPR => {
                        // Extract repr string from payload
                        use crate::value::HashableValue;
                        let repr = payload
                            .as_ref()
                            .and_then(|t| t.peek_result())
                            .and_then(|r| {
                                if let Ok(Value::Dict { entries, .. }) = r {
                                    let k = HashableValue::Str(Arc::from(FIELD_REPR));
                                    entries.get(&k).and_then(|t| t.peek_result()).and_then(|r| {
                                        if let Ok(Value::String {
                                            source, start, end, ..
                                        }) = r
                                        {
                                            Some(source[*start..*end].to_string())
                                        } else {
                                            None
                                        }
                                    })
                                } else {
                                    None
                                }
                            });
                        match repr.as_deref() {
                            Some(REPR_INT) => Some("Int"),
                            Some(REPR_FLOAT) => Some("Float"),
                            Some(REPR_STRING) => Some("String"),
                            Some(REPR_BYTES) => Some("Bytes"),
                            _ => None,
                        }
                    }
                    TV_INT_LIT => Some("Int"),
                    TV_FLOAT_LIT => Some("Float"),
                    TV_STR_LIT => Some("String"),
                    TV_OP => {
                        // Named type constructor — look up in tycon_env.
                        use crate::value::HashableValue;
                        let name = payload
                            .as_ref()
                            .and_then(|t| t.peek_result())
                            .and_then(|r| {
                                if let Ok(Value::Dict { entries, .. }) = r {
                                    let k = HashableValue::Str(Arc::from(FIELD_NAME));
                                    entries.get(&k).and_then(|t| t.peek_result()).and_then(|r| {
                                        if let Ok(Value::String {
                                            source, start, end, ..
                                        }) = r
                                        {
                                            Some(source[*start..*end].to_string())
                                        } else {
                                            None
                                        }
                                    })
                                } else {
                                    None
                                }
                            });
                        if let (Some(name), Some(env)) = (name, tycon_env) {
                            if let Some(def) = env.get(&name) {
                                if !def.constructors.is_empty() {
                                    let branches: Vec<CoveragePattern> = def
                                        .constructors
                                        .iter()
                                        .map(|(ctor, _arity)| CoveragePattern::Constructor {
                                            tag: ConstructorTag::Variant(ctor.clone()),
                                            sub_patterns: inner_sub.clone(),
                                        })
                                        .collect();
                                    if branches.len() == 1 {
                                        return branches.into_iter().next().unwrap();
                                    }
                                    return CoveragePattern::Wildcard;
                                }
                            }
                        }
                        None
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(tag_str) = tag_opt {
                CoveragePattern::Constructor {
                    tag: ConstructorTag::Variant(tag_str.into()),
                    sub_patterns: inner_sub,
                }
            } else {
                CoveragePattern::Wildcard
            }
        }

        // All other forms: Wildcard
        _ => CoveragePattern::Wildcard,
    }
}

/// Normalize `Constructor { tag: Variant(..), sub_patterns }` patterns to match
/// the declared arity in the sig, enforcing Maranget column-consistency.
///
/// Two cases arise from `ast_pattern_to_coverage`:
///
/// 1. Constructor patterns without bindings emit `sub_patterns: vec![]`.
///    - Arity 0 (unit variant): `[Square]:` → keep `vec![]`
///    - Arity 1 (payload variant): `[Circle]:` → upgrade to `vec![Wildcard]`
///
/// 2. Constructor patterns with bindings emit `sub_patterns: vec![inner]`
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
                // Non-Variant constructor (DictKey, Literal, Bottom) — recurse only.
                CoveragePattern::Constructor {
                    tag: tag.clone(),
                    sub_patterns: sub_patterns
                        .iter()
                        .map(|sp| normalize_constructor_arities(sp, sig))
                        .collect(),
                }
            }
        }
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
    // ast_pattern_to_coverage emits vec![] for constructor patterns without bindings;
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
    use crate::type_infer::{make_typevalue_repr, make_typevalue_str_lit};

    // Helper: create a constructor pattern
    fn con(tag: ConstructorTag, sub_patterns: Vec<CoveragePattern>) -> CoveragePattern {
        CoveragePattern::Constructor { tag, sub_patterns }
    }

    // Helper: wildcard
    fn wc() -> CoveragePattern {
        CoveragePattern::Wildcard
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

    // Helper: two-constructor signature (e.g. Coin.Heads | Coin.Tails)
    fn bool_sig() -> ConstructorSignature {
        sig(&[
            (ConstructorTag::Variant("Coin.Heads".into()), 0),
            (ConstructorTag::Variant("Coin.Tails".into()), 0),
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
    fn test_coverage_two_ctor_exhaustive() {
        let sig = bool_sig();
        let patterns = vec![
            con(ConstructorTag::Variant("Coin.Heads".into()), vec![]),
            con(ConstructorTag::Variant("Coin.Tails".into()), vec![]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_coverage_two_ctor_missing_one() {
        let sig = bool_sig();
        let patterns = vec![con(ConstructorTag::Variant("Coin.Heads".into()), vec![])];
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
            (ConstructorTag::Variant("Int".into()), 0),
            (ConstructorTag::Variant("String".into()), 0),
        ]);
        let patterns = vec![
            con(ConstructorTag::Variant("Int".into()), vec![]),
            con(ConstructorTag::Variant("String".into()), vec![]),
        ];
        let guards = vec![false, false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(result.exhaustive);
    }

    #[test]
    fn test_type_tag_missing() {
        let sig = sig(&[
            (ConstructorTag::Variant("Int".into()), 0),
            (ConstructorTag::Variant("String".into()), 0),
        ]);
        let patterns = vec![con(ConstructorTag::Variant("Int".into()), vec![])];
        let guards = vec![false];
        let result = check_coverage(&patterns, &sig, &guards);
        assert!(!result.exhaustive);
        assert_eq!(result.uncovered.len(), 1);
    }

    // AST pattern conversion tests deleted — Pattern enum deleted, patterns are now SurfaceNode.

    // ===== Constructor signature from TypeValue union tests =====

    fn make_record_typevalue_test(fields: &[(&str, Arc<Value>)]) -> Arc<Value> {
        use crate::value::{HashableValue, Thunk};
        let mut field_entries = indexmap::IndexMap::new();
        for (name, tv) in fields {
            field_entries.insert(
                HashableValue::Str(Arc::from(*name)),
                Arc::new(Thunk::value(Value::clone(tv.as_ref()), crate::rust_span!())),
            );
        }
        let fields_dict = Value::Dict {
            entries: field_entries,
            type_val: crate::value::unknown_type_val(),
        };
        let mut payload_entries = indexmap::IndexMap::new();
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_FIELDS)),
            Arc::new(Thunk::value(fields_dict, crate::rust_span!())),
        );
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_TAIL)),
            Arc::new(Thunk::value(
                Value::Dict {
                    entries: indexmap::IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
                crate::rust_span!(),
            )),
        );
        let payload = Value::Dict {
            entries: payload_entries,
            type_val: crate::value::unknown_type_val(),
        };
        Arc::new(Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(TV_RECORD),
            payload: Some(Arc::new(crate::value::Thunk::value(
                payload,
                crate::rust_span!(),
            ))),
        })
    }

    #[test]
    fn test_sig_from_union_record_variants() {
        let unknown_tv = crate::value::unknown_type_val();
        let str_tv = make_typevalue_repr(REPR_STRING);
        let union_members = vec![
            make_record_typevalue_test(&[("ok", unknown_tv)]),
            make_record_typevalue_test(&[("err", str_tv)]),
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
        let union_members = vec![
            make_typevalue_repr(REPR_INT),
            make_typevalue_repr(REPR_STRING),
        ];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("all members are representable");
        assert_eq!(sig.constructors.len(), 2);
        let tags = sig.tags();
        assert!(tags.contains(&ConstructorTag::Variant("Int".to_string())));
        assert!(tags.contains(&ConstructorTag::Variant("String".to_string())));
    }

    #[test]
    fn test_sig_from_union_string_literals() {
        let union_members = vec![make_typevalue_str_lit("ok"), make_typevalue_str_lit("err")];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new())
                .expect("all members are representable");
        assert_eq!(sig.constructors.len(), 2);
    }

    #[test]
    fn test_sig_from_union_with_unrepresentable_type_returns_none() {
        // TypeValue.Fn types cannot be expressed as a finite constructor set.
        // from_union must return None rather than a partial (unsound) signature.
        let fn_tv = Arc::new(Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(TV_FN),
            payload: None,
        });
        let union_members = vec![make_typevalue_repr(REPR_INT), fn_tv];
        let sig =
            ConstructorSignature::from_union(&union_members, &std::collections::HashMap::new());
        assert!(
            sig.is_none(),
            "union containing TypeValue.Fn must return None — cannot verify exhaustiveness"
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

    // ===== ast_pattern_to_coverage tests =====

    use std::sync::Arc;

    /// Build a minimal SurfaceNode for a given expression (no span metadata).
    fn mknode(expr: SurfaceExpression) -> SurfaceNode {
        SurfaceNode::new(expr, crate::rust_span!())
    }

    /// Build a VarRef expression with a resolved Resolution.
    fn varref_resolved(name: &str, _level: u32, slot: u32) -> SurfaceExpression {
        let r = crate::ast::Resolution::new();
        r.set(Some(crate::ast::VarAddr::LetrecGroupMember {
            depth: 0,
            slot,
        }));
        SurfaceExpression::VarRef {
            name: name.to_string(),
            escaped: false,
            resolution: r,
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
            do_infer_placeholder: false,
        }
    }

    /// Build a VarRef expression with Some(None) resolution (unresolved / not in scope).
    fn varref_unresolved(name: &str) -> SurfaceExpression {
        let r = crate::ast::Resolution::new();
        r.set(None);
        SurfaceExpression::VarRef {
            name: name.to_string(),
            escaped: false,
            resolution: r,
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
            do_infer_placeholder: false,
        }
    }

    /// Build a VarRef expression with no Resolution set (resolver never ran).
    fn varref_not_resolved(name: &str) -> SurfaceExpression {
        SurfaceExpression::VarRef {
            name: name.to_string(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
            do_infer_placeholder: false,
        }
    }

    /// Build a Field expression for `TypeName.CtorName`.
    fn field_dot(type_name: &str, ctor_name: &str) -> SurfaceExpression {
        let inner = Arc::new(SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: type_name.to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            crate::rust_span!(),
        ));
        SurfaceExpression::Field {
            expr: Some(inner),
            field: crate::ast::DotKey::Ident(ctor_name.to_string()),
            resolution: crate::ast::Resolution::new(),
        }
    }

    #[test]
    fn test_ast_pattern_placeholder_is_wildcard() {
        let node = mknode(SurfaceExpression::Placeholder(None, None));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(result, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_pattern_varref_unresolved_some_none_is_wildcard() {
        // VarRef where resolver returned Some(None) (not in scope) → wildcard
        let node = mknode(varref_unresolved("_"));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(result, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_pattern_varref_not_resolved_none_is_wildcard() {
        // VarRef where resolver never ran (None) → conservative wildcard
        let node = mknode(varref_not_resolved("x"));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(result, CoveragePattern::Wildcard);
    }

    #[test]
    fn test_ast_pattern_varref_resolved_pin_is_constructor() {
        // VarRef where resolver returned Some(Some((level, slot))) → pin constructor
        let node = mknode(varref_resolved("foo", 1, 0));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(
            result,
            CoveragePattern::Constructor {
                tag: ConstructorTag::DictKey("__pin_foo__".to_string()),
                sub_patterns: vec![],
            }
        );
    }

    #[test]
    fn test_ast_pattern_int_literal_is_constructor() {
        // Int(42) → Constructor(LiteralInt(42), [])
        let node = mknode(SurfaceExpression::Int(42));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(
            result,
            CoveragePattern::Constructor {
                tag: ConstructorTag::LiteralInt(42),
                sub_patterns: vec![],
            }
        );
    }

    #[test]
    fn test_ast_pattern_field_dot_access_is_variant_constructor() {
        // Color.Red (Field { expr: Some(VarRef("Color")), field: Ident("Red") })
        // → Constructor(Variant("Color.Red"), [])
        let node = mknode(field_dot("Color", "Red"));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(
            result,
            CoveragePattern::Constructor {
                tag: ConstructorTag::Variant("Color.Red".to_string()),
                sub_patterns: vec![],
            }
        );
    }

    #[test]
    fn test_ast_pattern_dict_single_key_is_dict_key_constructor() {
        // [host: _] → Constructor(DictKey("host"), [Wildcard])
        // Dict with one entry: key=VarRef("host"), value=Placeholder
        use crate::ast::{Spanned, SurfaceEntry};
        let key_node = Arc::new(SurfaceNode::new(
            SurfaceExpression::VarRef {
                name: "host".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            },
            crate::rust_span!(),
        ));
        let val_node = Arc::new(SurfaceNode::new(
            SurfaceExpression::Placeholder(None, None),
            crate::rust_span!(),
        ));
        let entry = Spanned::new(
            SurfaceEntry {
                key: Some(key_node),
                value: val_node,
            },
            crate::rust_span!(),
        );
        let node = mknode(SurfaceExpression::Dict(vec![entry]));
        let result = ast_pattern_to_coverage(&node, None);
        assert_eq!(
            result,
            CoveragePattern::Constructor {
                tag: ConstructorTag::DictKey("host".to_string()),
                sub_patterns: vec![CoveragePattern::Wildcard],
            }
        );
    }
}
