//! Evaluator error types with definition-site spans, materialization-site spans, and stack frames.

use std::fmt;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::ast::Span;

/// Convenience type alias for evaluation results.
pub type EvalResult<T> = Result<T, Box<EvalError>>;

/// Arity constraint for function calls.
#[derive(Debug, Clone, PartialEq)]
pub enum ArityBound {
    Exact(usize),
    Range(usize, usize),
}

impl fmt::Display for ArityBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(1) => write!(f, "1 argument"),
            Self::Exact(n) => write!(f, "{n} arguments"),
            Self::Range(lo, hi) => {
                if *lo == *hi {
                    // Range(n, n) is effectively Exact(n), so display as such
                    if *lo == 1 {
                        write!(f, "1 argument")
                    } else {
                        write!(f, "{lo} arguments")
                    }
                } else {
                    write!(f, "{lo} to {hi} arguments")
                }
            }
        }
    }
}

/// Blame polarity for gradual typing boundaries.
/// Determines which side of a typed/untyped boundary is responsible for a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameParity {
    /// Positive polarity: the typed side is blamed (value must conform to the declared type).
    /// Example: `[@Int x]` where `x` doesn't match — the TypeAssert annotation is responsible.
    Positive,
    /// Negative polarity: the untyped side is blamed (producer violated the contract).
    /// Example: function parameter receives wrong type — the call site is responsible.
    Negative,
}

/// Blame label for tracking typed/untyped boundaries in gradual typing.
/// Uses the co-natural strategy (Greenman et al. 2019): O(1) space per thunk,
/// innermost boundary label is preserved when values cross multiple boundaries.
#[derive(Debug, Clone, PartialEq)]
pub struct BlameLabel {
    /// Where the value originated (e.g., unannotated parameter, from-json result).
    pub origin_span: Span,
    /// Where the typed/untyped boundary was established (e.g., TypeAssert annotation site).
    pub boundary_span: Span,
    /// Which side of the boundary is responsible for conformance.
    pub polarity: BlameParity,
}

/// Pipeline blame provenance for contract violation enrichment.
/// Identifies the producing stage (positive party) and consuming stage (negative party)
/// per Findler & Felleisen (2002) contract blame semantics.
///
/// KNOWN ISSUE (BT4): PipelineBlame is defined but never instantiated. When a document
/// has a %@Type annotation (expects: field in SurfaceDocument), the pipeline should
/// construct PipelineBlame { producer: prev_stage_label, consumer: current_stage_label }
/// and thread it through the validation path. This requires:
/// 1. Tracking stage labels (document names or indices) during pipeline evaluation
/// 2. Passing PipelineBlame to wrap_with_nominal_validation in eval.rs
/// 3. Threading it through TypeAssert → GuardedValidate → validate_and_wrap_record
/// 4. Enriching type assertion errors with pipeline blame context
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineBlame {
    /// The producing stage label (positive party — blamed for wrong output shape).
    /// E.g., "data.llt" or "stage 0".
    pub producer: String,
    /// The consuming stage label (negative party — blamed for wrong contract).
    /// E.g., "transform.llt" or "stage 1".
    pub consumer: Option<String>,
}

/// Structured error kind with domain-specific data.
///
/// Note: `PartialEq` is implemented manually (not derived) to give
/// `FloatNotFinite` and `FloatOutOfRange` total equality semantics where
/// NaN == NaN. All other variants use structural field equality.
#[derive(Debug, Clone)]
pub enum ErrorKind {
    // --- Access errors (E000-E009) ---
    KeyNotFound {
        key: String,
        available_keys: Vec<String>,
    },
    /// `name` stores the identifier (bare name, no sigil prefix).
    /// Display shows the name as-is: `"undefined variable: x"`.
    /// For `%` pipeline refs, the name includes the `%`: `"undefined variable: %foo"`.
    UndefinedVariable {
        name: String,
    },

    // --- Type errors (E010-E019) ---
    /// Runtime type mismatch from evaluator or builtin dispatch.
    /// `context` carries the operation name (e.g., `"merge"`, `"document pipeline"`)
    /// when the mismatch originates from a named operation; `None` for generic mismatches.
    /// `expected` is human-readable, not machine-parseable — may contain
    /// compound descriptions like `"Dict or Seq"`.
    TypeMismatch {
        context: Option<String>,
        expected: String,
        got: String,
    },
    /// User-written type assertion (`[@Type value]`) failed at runtime.
    /// Semantically distinct from `TypeMismatch` — this is a user-authored
    /// type guard, not an internal evaluator check.
    TypeAssertFailed {
        expected: String,
        got: String,
    },
    /// No typeclass instance found for the given types.
    /// Raised when runtime dispatch fails to find a matching instance declaration.
    NoInstance {
        class_name: String,
        type_tags: Vec<String>,
    },
    /// Macro expansion error — validation failures, splice position errors, etc.
    /// Distinct from UserError because it occurs during the expansion pass, not evaluation.
    MacroError {
        message: String,
    },

    // --- Call errors (E020-E029) ---
    ArityMismatch {
        expected: ArityBound,
        got: usize,
    },
    MissingRequiredParam {
        param: String,
    },
    NamedArgConflict {
        param: String,
    },
    UnknownNamedArg {
        name: String,
        /// Names of all valid named parameters for this function (for error hint).
        valid_params: Vec<String>,
    },
    NamedArgRejected {
        builtin: String,
    },

    // --- Value errors (E030-E039) ---
    DuplicateKey {
        key: String,
    },
    /// `op` carries the operator symbol (e.g., `"/"`) for Display prefix.
    DivisionByZero {
        op: String,
    },
    IntegerOverflow {
        op: String,
    },
    /// Covers NaN, Infinity, and -Infinity — values that are not finite
    /// and cannot be converted to Int or used in contexts requiring finite floats.
    FloatNotFinite {
        builtin: String,
        value: f64,
    },
    EmptyCollection {
        op: String,
    },
    /// Runtime value type cannot be serialized to JSON (Function, Builtin, Seq, Proxy).
    /// `value_type` is the user-facing type name (e.g., "Function", "Proxy").
    ValueNotSerializable {
        value_type: String,
    },
    /// Float-to-int conversion failed: value is finite but outside i64 range.
    /// Distinct from `FloatNotFinite` (which rejects NaN/Infinity).
    FloatOutOfRange {
        builtin: String,
        value: f64,
    },

    // --- Limit errors (E041+) ---
    /// Resource limit exceeded (collection size, string size, etc.).
    /// This is non-catchable — resource limits are
    /// safety boundaries, not application-level errors.
    ResourceLimitExceeded {
        message: String,
    },
    /// Capability required but not provided (e.g., network access without --cap-net).
    /// User-actionable error indicating missing capability flag.
    CapabilityRequired {
        message: String,
    },

    // --- Include errors (E050-E059) ---
    /// Covers both "cannot open" (canonicalize failure) and "cannot read"
    /// (metadata/read failure). The `detail` field carries the OS error.
    IncludeIoError {
        path: String,
        detail: String,
    },
    IncludeCycle {
        path: String,
    },
    IncludeFileTooLarge {
        path: String,
        size: u64,
        limit: u64,
    },
    IncludeHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    IncludeHashRequired {
        path: String,
    },

    // --- Conversion errors (E060-E069) ---
    ParseConversion {
        builtin: String,
        input: String,
        target: String,
    },
    UriParseError {
        detail: String,
    },

    // --- Evaluation structure (E070-E079) ---
    CircularDependency {
        name: String,
        /// Full cycle chain: `[(origin_label, span), ...]`.
        /// Each entry represents a thunk in the evaluation chain leading to the cycle.
        /// Empty if cycle path tracking was disabled or the cycle was detected before
        /// the stack was populated.
        cycle_path: Vec<(Arc<str>, Span)>,
    },
    /// Non-exhaustive match: no pattern arm matched the scrutinee.
    /// `scrutinee_type` is the runtime type of the value that failed to match
    /// (e.g., `"Int"`, `"String"`, `"Dict"`).
    MatchExhaustion {
        scrutinee_type: String,
    },
    /// A variable name appears more than once in a single pattern.
    /// ML-family semantics require each variable to bind at most once per arm.
    /// E.g., `[a: x  b: x  ...]:` is illegal because `x` appears twice.
    DuplicateVariable {
        name: String,
    },

    /// User-generated (E080-E089)
    UserError {
        message: String,
    },
    /// Placeholder expression (`...`) reached during evaluation.
    /// Catchable via $try — allows placeholder values in configs.
    Unimplemented {
        message: String,
    },
    /// Operation attempted on a builder that has already been finished (frozen).
    /// Raised when user code calls builder-set/delete/get/has?/snapshot after
    /// builder-finish has been called.
    BuilderFinished {
        op: String,
    },

    /// Schema validation (E090-E094)
    SchemaViolation {
        /// List of (field_path, error_message) tuples for each violation.
        /// Field paths use dot notation: "user.address.zip"
        /// Note: ambiguous for keys containing `.` — documented limitation.
        violations: Vec<(String, String)>,
    },
    /// Type kind mismatch — a type constructor's kind does not match the expected kind.
    /// `expected` is the required kind (e.g., `"Type"` for a ground type, `"Type → Type"`
    /// for a unary type constructor). `got` is the actual kind produced by the expression.
    /// Raised when HKT annotations supply a kind-`*` type where `* → *` is required, or
    /// vice versa.
    KindMismatch {
        expected: String,
        got: String,
    },

    // --- Escape hatch (E095-E099) ---
    Internal {
        message: String,
    },
}

impl PartialEq for ErrorKind {
    /// Custom equality with total float semantics: NaN == NaN for float-containing
    /// variants (`FloatNotFinite`, `FloatOutOfRange`). All other variants use
    /// structural field equality equivalent to `#[derive(PartialEq)]`.
    fn eq(&self, other: &Self) -> bool {
        // Helper for f64 total equality: NaN == NaN, Inf == Inf, etc.
        #[inline]
        fn f64_total_eq(a: f64, b: f64) -> bool {
            a.to_bits() == b.to_bits()
        }

        match (self, other) {
            // --- Float variants with total equality ---
            (
                Self::FloatNotFinite {
                    builtin: b1,
                    value: v1,
                },
                Self::FloatNotFinite {
                    builtin: b2,
                    value: v2,
                },
            ) => b1 == b2 && f64_total_eq(*v1, *v2),
            (
                Self::FloatOutOfRange {
                    builtin: b1,
                    value: v1,
                },
                Self::FloatOutOfRange {
                    builtin: b2,
                    value: v2,
                },
            ) => b1 == b2 && f64_total_eq(*v1, *v2),

            // --- All other variants: structural field equality ---
            (
                Self::KeyNotFound {
                    key: k1,
                    available_keys: a1,
                },
                Self::KeyNotFound {
                    key: k2,
                    available_keys: a2,
                },
            ) => k1 == k2 && a1 == a2,
            (Self::UndefinedVariable { name: n1 }, Self::UndefinedVariable { name: n2 }) => {
                n1 == n2
            }
            (
                Self::TypeMismatch {
                    context: c1,
                    expected: e1,
                    got: g1,
                },
                Self::TypeMismatch {
                    context: c2,
                    expected: e2,
                    got: g2,
                },
            ) => c1 == c2 && e1 == e2 && g1 == g2,
            (
                Self::TypeAssertFailed {
                    expected: e1,
                    got: g1,
                },
                Self::TypeAssertFailed {
                    expected: e2,
                    got: g2,
                },
            ) => e1 == e2 && g1 == g2,
            (
                Self::NoInstance {
                    class_name: c1,
                    type_tags: t1,
                },
                Self::NoInstance {
                    class_name: c2,
                    type_tags: t2,
                },
            ) => c1 == c2 && t1 == t2,
            (Self::MacroError { message: m1 }, Self::MacroError { message: m2 }) => m1 == m2,
            (
                Self::ArityMismatch {
                    expected: e1,
                    got: g1,
                },
                Self::ArityMismatch {
                    expected: e2,
                    got: g2,
                },
            ) => e1 == e2 && g1 == g2,
            (
                Self::MissingRequiredParam { param: p1 },
                Self::MissingRequiredParam { param: p2 },
            ) => p1 == p2,
            (Self::NamedArgConflict { param: p1 }, Self::NamedArgConflict { param: p2 }) => {
                p1 == p2
            }
            (
                Self::UnknownNamedArg {
                    name: n1,
                    valid_params: v1,
                },
                Self::UnknownNamedArg {
                    name: n2,
                    valid_params: v2,
                },
            ) => n1 == n2 && v1 == v2,
            (Self::NamedArgRejected { builtin: b1 }, Self::NamedArgRejected { builtin: b2 }) => {
                b1 == b2
            }
            (Self::DuplicateKey { key: k1 }, Self::DuplicateKey { key: k2 }) => k1 == k2,
            (Self::DivisionByZero { op: o1 }, Self::DivisionByZero { op: o2 }) => o1 == o2,
            (Self::IntegerOverflow { op: o1 }, Self::IntegerOverflow { op: o2 }) => o1 == o2,
            (Self::EmptyCollection { op: o1 }, Self::EmptyCollection { op: o2 }) => o1 == o2,
            (
                Self::ValueNotSerializable { value_type: t1 },
                Self::ValueNotSerializable { value_type: t2 },
            ) => t1 == t2,
            (
                Self::ResourceLimitExceeded { message: m1 },
                Self::ResourceLimitExceeded { message: m2 },
            ) => m1 == m2,
            (
                Self::CapabilityRequired { message: m1 },
                Self::CapabilityRequired { message: m2 },
            ) => m1 == m2,
            (
                Self::IncludeIoError {
                    path: p1,
                    detail: d1,
                },
                Self::IncludeIoError {
                    path: p2,
                    detail: d2,
                },
            ) => p1 == p2 && d1 == d2,
            (Self::IncludeCycle { path: p1 }, Self::IncludeCycle { path: p2 }) => p1 == p2,
            (
                Self::IncludeFileTooLarge {
                    path: p1,
                    size: s1,
                    limit: l1,
                },
                Self::IncludeFileTooLarge {
                    path: p2,
                    size: s2,
                    limit: l2,
                },
            ) => p1 == p2 && s1 == s2 && l1 == l2,
            (
                Self::IncludeHashMismatch {
                    path: p1,
                    expected: e1,
                    actual: a1,
                },
                Self::IncludeHashMismatch {
                    path: p2,
                    expected: e2,
                    actual: a2,
                },
            ) => p1 == p2 && e1 == e2 && a1 == a2,
            (Self::IncludeHashRequired { path: p1 }, Self::IncludeHashRequired { path: p2 }) => {
                p1 == p2
            }
            (
                Self::ParseConversion {
                    builtin: b1,
                    input: i1,
                    target: t1,
                },
                Self::ParseConversion {
                    builtin: b2,
                    input: i2,
                    target: t2,
                },
            ) => b1 == b2 && i1 == i2 && t1 == t2,
            (Self::UriParseError { detail: d1 }, Self::UriParseError { detail: d2 }) => d1 == d2,
            (
                Self::CircularDependency {
                    name: n1,
                    cycle_path: c1,
                },
                Self::CircularDependency {
                    name: n2,
                    cycle_path: c2,
                },
            ) => n1 == n2 && c1 == c2,
            (Self::UserError { message: m1 }, Self::UserError { message: m2 }) => m1 == m2,
            (Self::Unimplemented { message: m1 }, Self::Unimplemented { message: m2 }) => m1 == m2,
            (Self::BuilderFinished { op: o1 }, Self::BuilderFinished { op: o2 }) => o1 == o2,
            (
                Self::SchemaViolation { violations: v1 },
                Self::SchemaViolation { violations: v2 },
            ) => v1 == v2,
            (
                Self::KindMismatch {
                    expected: e1,
                    got: g1,
                },
                Self::KindMismatch {
                    expected: e2,
                    got: g2,
                },
            ) => e1 == e2 && g1 == g2,
            (Self::Internal { message: m1 }, Self::Internal { message: m2 }) => m1 == m2,
            (
                Self::MatchExhaustion { scrutinee_type: t1 },
                Self::MatchExhaustion { scrutinee_type: t2 },
            ) => t1 == t2,
            (Self::DuplicateVariable { name: n1 }, Self::DuplicateVariable { name: n2 }) => {
                n1 == n2
            }

            // Different variants are never equal
            _ => false,
        }
    }
}

impl ErrorKind {
    /// Returns a stable error code string for this error kind.
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyNotFound { .. } => "E001",
            Self::UndefinedVariable { .. } => "E002",
            Self::TypeMismatch { .. } => "E010",
            Self::TypeAssertFailed { .. } => "E011",
            Self::NoInstance { .. } => "E013",
            Self::MacroError { .. } => "E012",
            Self::ArityMismatch { .. } => "E020",
            Self::MissingRequiredParam { .. } => "E024",
            Self::NamedArgConflict { .. } => "E021",
            Self::UnknownNamedArg { .. } => "E022",
            Self::NamedArgRejected { .. } => "E023",
            Self::DuplicateKey { .. } => "E030",
            Self::DivisionByZero { .. } => "E031",
            Self::IntegerOverflow { .. } => "E032",
            Self::FloatNotFinite { .. } => "E033",
            Self::EmptyCollection { .. } => "E034",
            Self::ValueNotSerializable { .. } => "E035",
            Self::FloatOutOfRange { .. } => "E036",
            Self::ResourceLimitExceeded { .. } => "E043",
            Self::CapabilityRequired { .. } => "E044",
            Self::IncludeIoError { .. } => "E051",
            Self::IncludeCycle { .. } => "E052",
            Self::IncludeFileTooLarge { .. } => "E054",
            Self::IncludeHashMismatch { .. } => "E055",
            Self::IncludeHashRequired { .. } => "E056",
            Self::ParseConversion { .. } => "E060",
            Self::UriParseError { .. } => "E063",
            Self::CircularDependency { .. } => "E070",
            Self::MatchExhaustion { .. } => "E071",
            Self::DuplicateVariable { .. } => "E072",
            Self::UserError { .. } => "E080",
            Self::Unimplemented { .. } => "E081",
            Self::BuilderFinished { .. } => "E082",
            Self::SchemaViolation { .. } => "E090",
            Self::KindMismatch { .. } => "E091",
            Self::Internal { .. } => "E099",
        }
    }

    /// Check if a name is a known builtin (for "did you mean string?" heuristic).
    /// This is a curated subset — not an exhaustive mirror of the `builtin_module()` groups.
    fn is_known_builtin(name: &str) -> bool {
        matches!(
            name,
            "+" | "-"
                | "*"
                | "/"
                | "="
                | "<"
                | "if"
                | "keys"
                | "length"
                | "merge"
                | "append"
                | "str"
                | "split"
                | "replace"
                | "upper"
                | "lower"
                | "trim"
                | "trim-start"
                | "trim-end"
                | "str-to-upper-char"
                | "str-to-lower-char"
                | "str-map-chars"
                | "str-index-of"
                | "regex-match?"
                | "floor"
                | "round"
                | "to-int"
                | "to-float"
                | "error"
                | "try"
                | "apply"
                | "until"
                | "type-of"
                | "from-json"
                | "include"
                | "seq"
                | "head"
                | "tail"
                | "collect"
                | "seq?"
                | "range"
                | "repeat"
                | "cycle"
                | "iterate"
                | "unfold"
                | "map"
                | "filter"
                | "take"
                | "drop"
                | "reduce"
                | "join"
                | "concat"
                | "rest"
                | "cons"
                | "reverse"
                | "sort"
                | "first"
                | "last"
                | "builtin-seq"
                | "builtin-head"
                | "builtin-tail"
                | "builtin-collect"
                | "builtin-range"
                | "builtin-repeat"
                | "builtin-cycle"
                | "builtin-iterate"
                | "builtin-unfold"
                | "builtin-join"
                | "builtin-concat"
                | "builtin-first"
                | "builtin-last"
                | "builtin-rest"
                | "builtin-cons"
                | "builtin-reverse"
                | "builtin-sort"
                | "proxy"
                // prelude-missing-wrappers: new builtin-* aliases
                | "builtin-keys"
                | "builtin-merge"
                | "builtin-each"
                | "builtin-each-key"
                | "builtin-each-kv"
                | "builtin-build-dict"
                | "builtin-floor"
                | "builtin-round"
                | "builtin-to-float"
                | "builtin-try"
                | "builtin-apply"
                | "builtin-type-of"
                | "builtin-narrow"
                | "builtin-from-json"
                // builtin-privacy-operators-and-io: new builtin-* aliases
                | "builtin-replace"
                | "builtin-str-chars"
                | "builtin-char-code"
                | "builtin-chr"
                | "builtin-str-bytes"
                | "builtin-bytes-str"
                | "builtin-str-index-of"
                | "builtin-trim-start"
                | "builtin-trim-end"
                | "builtin-str-to-upper-char"
                | "builtin-str-to-lower-char"
                | "builtin-str-map-chars"
                | "builtin-regex-match?"
                | "builtin-pow"
                | "builtin-sqrt"
                | "builtin-log"
                | "builtin-log2"
                | "builtin-log10"
                | "builtin-exp"
                | "builtin-sin"
                | "builtin-cos"
                | "builtin-tan"
                | "builtin-asin"
                | "builtin-acos"
                | "builtin-atan"
                | "builtin-atan2"
                | "builtin-nan?"
                | "builtin-inf?"
                | "builtin-finite?"
                | "builtin-band"
                | "builtin-bor"
                | "builtin-bxor"
                | "builtin-shl"
                | "builtin-shr"
                | "builtin-float"
                // B-168: I/O and builder builtins
                | "builtin-open"
                | "builtin-write"
                | "builtin-write-atomic"
                | "builtin-write-handle"
                | "builtin-flush"
                | "builtin-close"
                | "builtin-stat"
                | "builtin-exists"
                | "builtin-stat-symlink"
                | "builtin-copy-file"
                | "builtin-symlink"
                | "builtin-set-permissions"
                | "builtin-make-dir"
                | "builtin-rename"
                | "builtin-link"
                | "builtin-read-link"
                | "builtin-get-xattr"
                | "builtin-set-xattr"
                | "builtin-remove-xattr"
                | "builtin-list-xattrs"
                | "builtin-raw-create"
                | "builtin-seek"
                | "builtin-seek-end"
                | "builtin-position"
                | "builtin-revocable"
                | "builtin-revoke-cap"
                | "builtin-cap-data"
                | "builtin-connect"
                | "builtin-tls-layer"
                | "builtin-tls-peer-cert"
                | "builtin-send-datagram"
                | "builtin-recv-datagram"
                | "builtin-string-handle"
                | "builtin-make-builder"
                | "builtin-builder-set"
                | "builtin-builder-delete"
                | "builtin-builder-finish"
                | "builtin-builder-snapshot"
                | "builtin-builder-has?"
                | "builtin-builder-get"
                | "builtin-builder-get-or"
                | "builtin-expand"
                | "builtin-load"
                | "builtin-eval"
                | "builtin-eval-types"
                | "builtin-blake3"
                | "builtin-cap-identity"
                | "builtin-include-cache-get"
                | "builtin-include-cache-put"
        )
    }

    /// Returns `false` for errors that must not be cached in Failed thunk state.
    /// All errors are cacheable — a failed thunk is always stored in Failed state
    /// and subsequent accesses return the cached error without re-evaluation.
    ///
    /// # INVARIANT
    /// This method and `is_catchable()` serve distinct semantic roles:
    /// - **Cacheability**: Enforces Launchbury (1993) thunk state machine
    ///   monotonicity. All errors transition to Failed state and are memoized.
    /// - **Catchability**: Defines user-facing `try` semantics per Nix `tryEval`
    ///   model. Non-catchable errors propagate to the runtime regardless of
    ///   try/catch constructs.
    ///
    /// Cross-reference: see `is_catchable()` for `try` semantics.
    pub fn is_cacheable(&self) -> bool {
        true
    }

    /// Returns `false` for errors that must not be caught by `try`.
    /// Resource limit errors (`ResourceLimitExceeded`) should propagate to the
    /// runtime, not be suppressible by user code.
    /// Follows GHC's StackOverflow and Racket's exn:fail:resource semantics.
    ///
    /// # INVARIANT
    /// This method and `is_cacheable()` serve distinct semantic roles:
    /// `ResourceLimitExceeded` is non-catchable (resource limits are safety
    /// boundaries) but IS cacheable (hitting a limit will always fail again —
    /// deterministic).
    /// - **Catchability**: Defines user-facing `try` semantics per Nix `tryEval`
    ///   model. Non-catchable errors propagate to the runtime regardless of
    ///   try/catch constructs.
    /// - **Cacheability**: Enforces Launchbury (1993) thunk state machine
    ///   monotonicity. All errors transition to Failed state and are memoized.
    ///
    /// Cross-reference: see `is_cacheable()` for thunk state machine semantics.
    pub fn is_catchable(&self) -> bool {
        !matches!(self, Self::ResourceLimitExceeded { .. })
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyNotFound {
                key,
                available_keys,
            } => {
                write!(f, "key not found: {key}")?;
                if available_keys.is_empty() {
                    return Ok(());
                }

                // Find best match using Jaro-Winkler similarity.
                // Threshold 0.8 is consistent with prior art (cargo, rustc suggest at ~0.8).
                // Jaro-Winkler is prefix-weighted and handles transpositions well for short
                // identifier-style keys. A single-char typo on a 4-char key typically scores
                // 0.83-0.92; unrelated keys typically score < 0.7.
                let mut best_match: Option<(&str, f64)> = None;
                for avail in available_keys {
                    let similarity = strsim::jaro_winkler(key, avail);
                    if similarity > 0.8 {
                        if let Some((_, best_sim)) = best_match {
                            if similarity > best_sim {
                                best_match = Some((avail, similarity));
                            }
                        } else {
                            best_match = Some((avail, similarity));
                        }
                    }
                }

                if let Some((suggestion, _)) = best_match {
                    write!(f, " (did you mean: '{suggestion}')")
                } else {
                    // No close match found above the similarity threshold.
                    // Fall back to listing up to 5 available keys so the user
                    // can see what keys are actually present. Mirrors rustc's
                    // "available fields are: ..." style for struct field errors.
                    let keys_str = available_keys
                        .iter()
                        .take(5)
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let suffix = if available_keys.len() > 5 {
                        ", ..."
                    } else {
                        ""
                    };
                    write!(f, " (available keys: {keys_str}{suffix})")
                }
            }
            Self::UndefinedVariable { name } => {
                // Phase 2: bare identifiers are references, no $ prefix in display.
                // Check if this looks like an intended string literal (heuristic).
                let looks_like_string = !name.starts_with('%')
                    // name never starts with '$' (EscapedRef stores bare name without sigil)
                    && !name.starts_with('$')
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !Self::is_known_builtin(name);

                if looks_like_string {
                    write!(f, "undefined variable: {name} (did you mean the string \"{name}\"? Use quotes.)")
                } else {
                    write!(f, "undefined variable: {name}")
                }
            }
            Self::TypeMismatch {
                context: Some(ctx),
                expected,
                got,
            } => write!(f, "{ctx}: expected {expected}, got {got}"),
            Self::TypeMismatch {
                context: None,
                expected,
                got,
            } => write!(f, "type mismatch: expected {expected}, got {got}"),
            Self::TypeAssertFailed { expected, got } => {
                write!(f, "type assertion failed: expected {expected}, got {got}")
            }
            Self::NoInstance {
                class_name,
                type_tags,
            } => {
                let types_str = type_tags.join(", ");
                write!(f, "no instance for {class_name} ({types_str})")
            }
            Self::MacroError { message } => {
                write!(f, "macro expansion error: {message}")
            }
            Self::ArityMismatch { expected, got } => {
                write!(f, "arity mismatch: expected {expected}, got {got}")
            }
            Self::MissingRequiredParam { param } => {
                write!(f, "missing argument for required parameter '{param}'")
            }
            Self::NamedArgConflict { param } => write!(
                f,
                "parameter '{param}' received both positional and named argument"
            ),
            Self::UnknownNamedArg { name, valid_params } => {
                if valid_params.is_empty() {
                    write!(
                        f,
                        "unexpected named argument: {name} (function has no parameters)"
                    )
                } else {
                    let valid = valid_params.join(", ");
                    write!(
                        f,
                        "unexpected named argument: {name} (valid parameter names: {valid})"
                    )
                }
            }
            Self::NamedArgRejected { builtin } => {
                write!(f, "{builtin} does not accept named arguments")
            }
            Self::DuplicateKey { key } => write!(f, "duplicate key: {key}"),
            Self::DivisionByZero { op } => write!(f, "{op}: division by zero"),
            Self::IntegerOverflow { op } => write!(f, "{op}: integer overflow"),
            Self::FloatNotFinite { builtin, value } => {
                write!(f, "{builtin}: {value} is not a finite number")
            }
            Self::EmptyCollection { op } => write!(f, "{op} on empty collection"),
            Self::ValueNotSerializable { value_type } => {
                write!(f, "cannot serialize {value_type} to JSON")
            }
            Self::FloatOutOfRange { builtin, value } => {
                write!(f, "{builtin}: {value} is out of range for Int")
            }
            Self::ResourceLimitExceeded { message } => write!(f, "{}", message),
            Self::CapabilityRequired { message } => write!(f, "{}", message),
            Self::IncludeIoError { path, detail } => {
                write!(f, "include: cannot access \"{path}\": {detail}")
            }
            Self::IncludeCycle { path } => write!(f, "circular include detected: \"{path}\""),
            Self::IncludeFileTooLarge { path, size, limit } => write!(
                f,
                "include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"
            ),
            Self::IncludeHashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "include: integrity check failed for \"{path}\": expected {expected}, got {actual}"
            ),
            Self::IncludeHashRequired { path } => write!(
                f,
                "include: integrity hash required for \"{path}\" (--require-integrity)"
            ),
            Self::ParseConversion {
                builtin,
                input,
                target,
            } => write!(f, "{builtin}: cannot parse {input:?} as {target}"),
            Self::UriParseError { detail } => write!(f, "URI parse error: {detail}"),
            Self::CircularDependency { name, cycle_path } => {
                write!(f, "circular dependency detected while evaluating {name}")?;
                if !cycle_path.is_empty() {
                    write!(f, "\n  cycle:")?;
                    for (label, span) in cycle_path {
                        write!(f, " {} ({})", label, span)?;
                        write!(f, " →")?;
                    }
                    write!(f, " [back to {}]", name)?;
                }
                Ok(())
            }
            Self::MatchExhaustion { scrutinee_type } => {
                write!(
                    f,
                    "non-exhaustive match: no pattern matched the {scrutinee_type} value"
                )
            }
            Self::DuplicateVariable { name } => {
                write!(
                    f,
                    "duplicate variable in pattern: '{name}' appears more than once"
                )
            }
            Self::UserError { message } => write!(f, "{message}"),
            Self::Unimplemented { message } => write!(f, "{message}"),
            Self::BuilderFinished { op } => {
                write!(f, "{op}: builder has already been finished")
            }
            Self::SchemaViolation { violations } => {
                writeln!(
                    f,
                    "schema validation failed with {} error(s):",
                    violations.len()
                )?;
                for (field, msg) in violations {
                    writeln!(f, "  {}: {}", field, msg)?;
                }
                Ok(())
            }
            Self::KindMismatch { expected, got } => {
                write!(f, "kind mismatch: expected {expected}, got {got}")
            }
            Self::Internal { message } => write!(f, "{message}"),
        }
    }
}

/// A single frame in an evaluation stack trace (function name + source locations).
///
/// Carries two spans to support the dual-span error model:
/// - `definition_span`: where the thunk was defined (the origin of the value being forced)
/// - `materialization_span`: where the thunk was forced (the call/access site)
///
/// Both spans may carry `file: Option<Arc<SourceFile>>` so that rich source snippets
/// can be rendered directly from the frame without a separate source-file registry.
#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    pub label: String,
    /// Where the expression being forced was defined.
    pub definition_span: Span,
    /// Where the expression was forced (call/access site).
    pub materialization_span: Span,
}

/// Evaluation error with definition-site span, optional materialization-site span,
/// and a stack trace of enclosing function calls.
///
/// Source file information is embedded in each span's `file: Option<Arc<SourceFile>>` field
/// (populated by the parser for source-backed spans). There is no separate `source_file`
/// field — use `self.definition_span.file` to obtain the file path and content for rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub kind: ErrorKind,
    pub definition_span: Span,
    pub materialization_span: Option<Span>,
    pub stack: SmallVec<[StackFrame; 8]>,
    /// Optional secondary span with a label, e.g. "evaluated to Bool" pointing at a value site.
    /// Displayed after the primary error line when present.
    pub secondary_span: Option<(Span, String)>,
    /// Optional macro expansion provenance: (macro_name, call_site_span).
    /// When set, the error Display shows "in expansion of `<name>` at line:col".
    /// Populated by the error propagation path when errors occur in macro-expanded code.
    /// See Pombrio & Krishnamurthi (2015) for the "honest tags" approach to expansion provenance.
    pub macro_expansion: Option<(String, Span)>,
    /// Optional blame label for gradual typing boundaries.
    /// When present, identifies the typed/untyped boundary responsible for a type error.
    pub blame: Option<BlameLabel>,
    /// Optional pipeline blame provenance for contract violation enrichment.
    /// When present, identifies the producing/consuming stage at a `---` boundary.
    pub pipeline_stage: Option<PipelineBlame>,
}

impl EvalError {
    /// Create an error with the Internal escape hatch kind.
    /// Use typed ErrorKind variants instead when possible.
    pub fn internal(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::Internal { message },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    /// Create a BuilderFinished error for a specific operation.
    /// Used when a builder operation is attempted after builder-finish.
    pub fn builder_already_finished(op: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::BuilderFinished { op: op.into() },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn with_materialization_span(mut self, span: Span) -> Self {
        self.materialization_span = Some(span);
        self
    }

    /// Builder for stack frame attachment.
    pub fn with_frame(mut self, label: String, span: Span) -> Self {
        self.stack.push(StackFrame {
            label,
            definition_span: span.clone(),
            materialization_span: span,
        });
        self
    }

    /// Mutable stack frame push.
    pub fn push_frame(&mut self, label: String, span: Span) {
        self.stack.push(StackFrame {
            label,
            definition_span: span.clone(),
            materialization_span: span,
        });
    }

    /// Mutable stack frame push with separate definition and materialization spans.
    /// Use this when the thunk's definition site differs from the call/access site.
    pub fn push_frame_dual(
        &mut self,
        label: String,
        definition_span: Span,
        materialization_span: Span,
    ) {
        self.stack.push(StackFrame {
            label,
            definition_span,
            materialization_span,
        });
    }

    /// Builder for attaching a secondary span label, e.g. `"evaluated to Bool"`.
    /// The secondary span is displayed on a separate line after the primary error,
    /// pointing at a related source location (e.g. where a value was defined).
    pub fn with_secondary_span(mut self, span: Span, label: impl Into<String>) -> Self {
        self.secondary_span = Some((span, label.into()));
        self
    }

    /// Builder for attaching macro expansion provenance.
    /// Shows "in expansion of `<name>` at line:col" in the error output.
    pub fn with_macro_expansion(mut self, macro_name: String, call_site: Span) -> Self {
        self.macro_expansion = Some((macro_name, call_site));
        self
    }

    /// Attach a blame label for gradual typing boundary errors.
    /// Uses co-natural strategy: innermost blame label is preserved, outer labels discarded.
    pub fn with_blame(mut self, label: BlameLabel) -> Self {
        // Co-natural strategy: if we already have a blame label (innermost boundary),
        // keep it and discard the new outer label.
        if self.blame.is_none() {
            self.blame = Some(label);
        }
        self
    }

    /// Attach pipeline blame provenance for contract violation errors.
    /// Identifies the producing stage (positive party) and consuming stage (negative party).
    pub fn with_pipeline_blame(mut self, blame: PipelineBlame) -> Self {
        if self.pipeline_stage.is_none() {
            self.pipeline_stage = Some(blame);
        }
        self
    }

    pub fn key_not_found(key: &str, available_keys: Vec<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::KeyNotFound {
                key: key.to_string(),
                available_keys,
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn type_mismatch(expected: &str, got: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::TypeMismatch {
                context: None,
                expected: expected.to_string(),
                got: got.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn arity_mismatch(expected: usize, got: usize, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ArityMismatch {
                expected: ArityBound::Exact(expected),
                got,
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn arity_mismatch_bound(expected: ArityBound, got: usize, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ArityMismatch { expected, got },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn circular_dependency(
        name: &str,
        definition_span: Span,
        cycle_path: Vec<(Arc<str>, Span)>,
    ) -> Self {
        Self {
            kind: ErrorKind::CircularDependency {
                name: name.to_string(),
                cycle_path,
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn match_exhaustion(scrutinee_type: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::MatchExhaustion {
                scrutinee_type: scrutinee_type.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn duplicate_variable_in_pattern(name: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DuplicateVariable {
                name: name.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn capability_required(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::CapabilityRequired { message },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn user_error(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UserError { message },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn macro_error(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::MacroError { message },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn unimplemented(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::Unimplemented { message },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn schema_violation(violations: Vec<(String, String)>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::SchemaViolation { violations },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn kind_mismatch(expected: &str, got: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::KindMismatch {
                expected: expected.to_string(),
                got: got.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn named_arg_rejected(builtin: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NamedArgRejected { builtin },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn integer_overflow(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IntegerOverflow { op },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn type_mismatch_ctx(
        context: String,
        expected: &str,
        got: &str,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::TypeMismatch {
                context: Some(context),
                expected: expected.to_string(),
                got: got.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn division_by_zero(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DivisionByZero { op },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn float_not_finite(builtin: String, value: f64, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::FloatNotFinite { builtin, value },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn empty_collection(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::EmptyCollection { op },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn value_not_serializable(value_type: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ValueNotSerializable { value_type },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn float_out_of_range(builtin: String, value: f64, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::FloatOutOfRange { builtin, value },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn undefined_variable(name: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UndefinedVariable { name },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn type_assert_failed(expected: &str, got: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::TypeAssertFailed {
                expected: expected.to_string(),
                got: got.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn no_instance(class_name: &str, type_tags: Vec<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NoInstance {
                class_name: class_name.to_string(),
                type_tags,
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn named_arg_conflict(param: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NamedArgConflict { param },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn unknown_named_arg(
        name: String,
        valid_params: Vec<String>,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::UnknownNamedArg { name, valid_params },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn duplicate_key(key: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DuplicateKey {
                key: key.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn resource_limit_exceeded(message: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ResourceLimitExceeded {
                message: message.into(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn include_io_error(path: String, detail: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeIoError { path, detail },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn include_cycle(path: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeCycle { path },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn include_file_too_large(
        path: String,
        size: u64,
        limit: u64,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::IncludeFileTooLarge { path, size, limit },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn include_hash_mismatch(
        path: String,
        expected: String,
        actual: String,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::IncludeHashMismatch {
                path,
                expected,
                actual,
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn include_hash_required(path: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeHashRequired { path },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn parse_conversion(
        builtin: String,
        input: String,
        target: &str,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::ParseConversion {
                builtin,
                input,
                target: target.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn uri_parse_error(detail: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UriParseError { detail },
            definition_span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }

    pub fn missing_required_param(param: impl Into<String>, span: Span) -> Self {
        Self {
            kind: ErrorKind::MissingRequiredParam {
                param: param.into(),
            },
            definition_span: span,
            materialization_span: None,
            stack: SmallVec::new(),
            secondary_span: None,
            macro_expansion: None,
            blame: None,
            pipeline_stage: None,
        }
    }
}

/// Returns `true` if the stack frame should appear in user-facing error output.
/// Returns `false` only for synthetic origin spans (Span::origin() = offset 0, line 1, col 1)
/// from stdlib/builtin calls — these have no meaningful source location.
///
/// NOTE: No suffix-based filtering is applied. Every frame with a real source location
/// is shown, including stdlib internal helpers (-impl, -step, -check, -merge). This is
/// necessary for diagnosing bugs in macro transformers and stdlib code.
fn should_display_frame(frame: &StackFrame) -> bool {
    frame.definition_span != Span::origin()
}

/// Infer a context-appropriate verb for the materialization span label.
/// Checks the first visible stack frame label to determine whether the thunk
/// was forced by a function call or a field access.
///
/// Phase 2 frame formats:
/// - `"[name ...]"` → function call → "called at"
/// - `"accessing .field"` / `"accessing [..]"` / `"accessing [..:..]"` → "accessed at"
fn infer_materialization_verb(stack: &[StackFrame]) -> &'static str {
    for frame in stack {
        if !should_display_frame(frame) {
            continue;
        }
        let label = &frame.label;
        // Phase 2: implied-call frames are "[name ...]" — always a call
        if label.starts_with('[') {
            return "called at";
        }
        // Access frames: "accessing .field", "accessing [..]", "accessing [..:..]"
        if label.contains("access") || label.contains('.') || label.contains("bracket") {
            return "accessed at";
        }
    }
    "materialized at"
}

/// Format a span location string, prefixing with the file path when available.
/// Used for both the primary error location and stack frame locations.
///
/// When the span carries an embedded `SourceFile`, formats as `"path:line:col-line:col"`.
/// Otherwise formats as `"line:col-line:col"` (position only).
fn format_span_location(span: &Span) -> String {
    if let Some(ref sf) = span.file {
        format!("{}:{}", sf.path, span)
    } else {
        format!("{}", span)
    }
}

/// Write a source snippet for a stack frame's span, if the span carries file content.
/// Outputs `\n   |\n{snippet}` — same style as the primary error snippet but for frames.
fn write_frame_snippet(f: &mut fmt::Formatter<'_>, span: &Span) -> fmt::Result {
    if let Some(ref sf) = span.file {
        if let Some(snippet) = render_span_snippet(&sf.content, span.clone()) {
            write!(f, "\n   |")?;
            for line in snippet.lines() {
                write!(f, "\n{}", line)?;
            }
        }
    }
    Ok(())
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Primary error line: "[Exx] message (defined at file:line:col)"
        // File path comes from the embedded SourceFile in definition_span, not a separate field.
        write!(
            f,
            "[{}] {} (defined at {})",
            self.kind.code(),
            self.kind,
            format_span_location(&self.definition_span),
        )?;

        // Only show materialization span if it differs from definition span (doc/10-errors.md:820)
        // Written here (inline on the first line) before the snippet block.
        if let Some(ref mat_span) = self.materialization_span {
            if mat_span != &self.definition_span {
                let verb = infer_materialization_verb(&self.stack);
                write!(f, " ({verb} {})", format_span_location(mat_span))?;
            }
        }

        // Source snippet: rustc-style caret underlining, shown when the span carries source content.
        // Only rendered when definition_span has an embedded SourceFile (parser-backed spans).
        // Positioned after the first line so the snippet does not disrupt span location inline text.
        if let Some(ref sf) = self.definition_span.file {
            if let Some(snippet) = render_span_snippet(&sf.content, self.definition_span.clone()) {
                write!(f, "\n  --> {}:{}", sf.path, self.definition_span)?;
                write!(f, "\n   |")?;
                // render_span_snippet returns lines like "  N | line" and "  N | ^^^"
                for line in snippet.lines() {
                    write!(f, "\n{}", line)?;
                }
            }
        }

        // Secondary span: "evaluated to X" label pointing at a related source location.
        if let Some((ref sec_span, ref sec_label)) = self.secondary_span {
            write!(
                f,
                "\n  note: {sec_label} at {}",
                format_span_location(sec_span)
            )?;
        }

        // Display all visible stack frames
        for frame in &self.stack {
            if !should_display_frame(frame) {
                continue;
            }

            let loc = format_span_location(&frame.definition_span);
            write!(f, "\n  in {} at {}", frame.label, loc)?;
            write_frame_snippet(f, &frame.definition_span)?;
        }

        // Macro expansion provenance: shows "in expansion of `<name>` at line:col"
        // when the error occurred in macro-generated code. This is dual-span reporting
        // per Pombrio & Krishnamurthi (2015).
        if let Some((ref macro_name, ref call_site)) = self.macro_expansion {
            write!(
                f,
                "\n  in expansion of `{}` at {}:{}",
                macro_name, call_site.start.line, call_site.start.column
            )?;
        }

        // Blame label: gradual typing boundary provenance
        if let Some(ref blame) = self.blame {
            let party = match blame.polarity {
                BlameParity::Positive => "typed side (annotation)",
                BlameParity::Negative => "untyped side (producer)",
            };
            write!(
                f,
                "\n  blame: value from {} crossed boundary at {} ({} responsible)",
                blame.origin_span, blame.boundary_span, party
            )?;
        }

        // Pipeline blame: contract violation provenance at --- boundaries
        if let Some(ref pb) = self.pipeline_stage {
            write!(f, "\n  produced by: {}", pb.producer)?;
            if let Some(ref consumer) = pb.consumer {
                write!(f, "\n  consumed by: {}", consumer)?;
            }
            // Hint based on error kind: suggest fix direction
            match &self.kind {
                ErrorKind::TypeAssertFailed { .. } | ErrorKind::TypeMismatch { .. } => {
                    write!(
                        f,
                        "\n  hint: fix the producing stage or add a type cast in the consuming stage"
                    )?;
                }
                ErrorKind::SchemaViolation { .. } => {
                    write!(
                        f,
                        "\n  hint: fix the producing stage to match the schema contract"
                    )?;
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl std::error::Error for EvalError {}

// ────────────────────────────────────────────────────────────────────────────
// Type diagnostic system (three-tier: Info, Warn, Err)
// ────────────────────────────────────────────────────────────────────────────

/// Diagnostic severity level for type checking notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// Hint or suggestion (dimmed in CLI, Hint in LSP)
    Info,
    /// Concern (yellow in CLI, Warning in LSP)
    Warn,
    /// Fatal error (red in CLI, Error in LSP; causes non-zero exit)
    Err,
}

impl DiagnosticLevel {
    /// Bump severity one level: Info→Warn, Warn→Err, Err→Err.
    /// Used when `--strict` mode is enabled.
    pub fn bump(self) -> DiagnosticLevel {
        match self {
            DiagnosticLevel::Info => DiagnosticLevel::Warn,
            DiagnosticLevel::Warn => DiagnosticLevel::Err,
            DiagnosticLevel::Err => DiagnosticLevel::Err,
        }
    }
}

/// A type checking diagnostic (info/warn/err) with span and error code.
///
/// Unlike `TypeError` (which is always fatal), `TypeDiagnostic` can have
/// Info/Warn level for non-fatal notifications (e.g., inferred `Unknown` types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDiagnostic {
    pub message: String,
    pub span: crate::ast::Span,
    pub code: &'static str,
    pub level: DiagnosticLevel,
}

/// Render a source snippet with caret annotations for the given span.
///
/// Returns `None` for synthetic spans (`Span::origin()` at 0:0-0:0), which have no source line.
/// Returns `None` if the span is out of bounds for the source.
///
/// # Design note: hand-rolled vs `codespan-reporting`
///
/// The `codespan-reporting` crate (0.11.x) provides a more featureful renderer but requires
/// a `Files` registry of byte-indexed source strings, a `FileId` handle type, and ANSI color
/// support. For tinct's use case the overhead is not justified:
///
/// - Tinct operates on a **single source string** per error (REPL input, CLI file, LSP document).
///   There is no multi-file context to register.
/// - The output needs to be embeddable as a `String` inside `DiagnosticRelatedInformation.message`
///   (LSP path) and REPL error strings — raw ANSI codes in those contexts are noise.
/// - The hand-rolled approach is ~90 lines and covers both single-line and multi-line spans with
///   the format tinct already uses (`N | line` gutter).
///
/// Revisit if tinct adds multi-file include error rendering (e.g., showing both the include
/// call site and the error inside the included file), where `codespan-reporting`'s multi-file
/// support would provide real value.
///
/// For single-line spans (most common), shows:
/// ```text
///    {line_num} | {source_line}
///               | {carets}
/// ```
///
/// For multi-line spans, shows all lines with carets:
/// ```text
///    {first_line_num} | {first_line_from_start_col}
///    {mid_line_num}   | {middle_line}
///    {last_line_num}  | {last_line_to_end_col}
///                     | {carets}
/// ```
///
/// This is a rustc-style snippet renderer. Callers hold the source text (REPL has `input: &str`,
/// CLI has the file contents) and pass it alongside the error's `definition_span`.
pub fn render_span_snippet(source: &str, span: Span) -> Option<String> {
    // Suppress synthetic spans (Span::origin() is 0:0-0:0)
    if span.start.offset == 0 && span.end.offset == 0 {
        return None;
    }

    // Split source into lines
    let lines: Vec<&str> = source.lines().collect();

    // Span uses 1-based line numbers
    if span.start.line < 1 || span.start.line > lines.len() {
        return None; // Span out of bounds
    }

    // Width of the largest line number shown, for consistent gutter alignment.
    let end_line = span.end.line.min(lines.len());
    let line_num_width = end_line.to_string().len();
    let padding = " ".repeat(line_num_width);

    let mut snippet = String::new();

    if span.start.line == end_line {
        // ── Single-line span ──────────────────────────────────────────────
        let line_text = lines[span.start.line - 1];
        let start_col = span.start.column.saturating_sub(1).min(line_text.len());
        let end_col = span.end.column.saturating_sub(1).min(line_text.len());
        let caret_length = if end_col > start_col {
            end_col - start_col
        } else {
            1
        };

        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            span.start.line,
            line_text,
            width = line_num_width
        ));
        snippet.push_str(&format!("  {} | ", padding));
        snippet.push_str(&" ".repeat(start_col));
        snippet.push_str(&"^".repeat(caret_length));
    } else {
        // ── Multi-line span ───────────────────────────────────────────────
        let first_line = lines[span.start.line - 1];
        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            span.start.line,
            first_line,
            width = line_num_width
        ));

        // Middle lines: show full line content (no caret yet).
        for line_num in (span.start.line + 1)..end_line {
            let line_text = lines[line_num - 1];
            snippet.push_str(&format!(
                "  {:>width$} | {}\n",
                line_num,
                line_text,
                width = line_num_width
            ));
        }

        // Last line: show from col 0 to end_col, with caret under it.
        let last_line = lines[end_line - 1];
        let end_col = span.end.column.saturating_sub(1).min(last_line.len());
        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            end_line,
            last_line,
            width = line_num_width
        ));

        // Caret line: underline the last line from col 0 to end_col.
        // Using start_col from the first line would misalign when the last line starts
        // at a different column (e.g. fn body indented differently from fn header).
        let caret_length = end_col.max(1);
        snippet.push_str(&format!("  {} | ", padding));
        snippet.push_str(&"^".repeat(caret_length));
    }

    Some(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_span;

    #[test]
    fn test_eval_error_new() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::internal("something broke".to_string(), span.clone());
        assert_eq!(err.kind.to_string(), "something broke");
        assert_eq!(err.definition_span, span);
        assert_eq!(err.materialization_span, None);
        assert!(err.stack.is_empty());
    }

    #[test]
    fn test_eval_error_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err = EvalError::internal("lazy fail".to_string(), def_span)
            .with_materialization_span(mat_span.clone());
        assert_eq!(err.materialization_span, Some(mat_span));
    }

    #[test]
    fn test_eval_error_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = EvalError::internal("err".to_string(), span)
            .with_frame("my_function".to_string(), frame_span.clone());
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "my_function");
        assert_eq!(err.stack[0].definition_span, frame_span);
    }

    #[test]
    fn test_eval_error_key_not_found() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::key_not_found("foo", vec![], span);
        assert_eq!(err.kind.to_string(), "key not found: foo");
    }

    #[test]
    fn test_eval_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::type_mismatch("Int", "String", span);
        assert_eq!(
            err.kind.to_string(),
            "type mismatch: expected Int, got String"
        );
    }

    #[test]
    fn test_eval_error_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::arity_mismatch(2, 3, span);
        assert_eq!(
            err.kind.to_string(),
            "arity mismatch: expected 2 arguments, got 3"
        );
    }

    #[test]
    fn test_eval_error_circular_dependency() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::circular_dependency("x", span, Vec::new());
        assert_eq!(
            err.kind.to_string(),
            "circular dependency detected while evaluating x"
        );
    }

    #[test]
    fn test_eval_error_display_basic() {
        let span = test_span(3, 5, 3, 10);
        let err = EvalError::internal("oops".to_string(), span);
        let display = format!("{err}");
        assert_eq!(display, "[E099] oops (defined at 3:5-3:10)");
    }

    #[test]
    fn test_eval_error_display_full() {
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let frame1_span = test_span(10, 2, 10, 8);
        let frame2_span = test_span(15, 1, 15, 12);
        let err = EvalError::internal("bad value".to_string(), def_span)
            .with_materialization_span(mat_span)
            .with_frame("outer".to_string(), frame1_span)
            .with_frame("inner".to_string(), frame2_span);
        let display = format!("{err}");
        let expected = "\
[E099] bad value (defined at 3:5-3:10) (materialized at 20:1-20:5)
  in outer at 10:2-10:8
  in inner at 15:1-15:12";
        assert_eq!(display, expected);
    }

    #[test]
    fn test_eval_error_push_frame() {
        let span = test_span(1, 1, 1, 5);
        let mut err = EvalError::internal("error".to_string(), span);

        assert!(err.stack.is_empty());

        // Push a frame directly
        let frame_span = test_span(5, 1, 5, 10);
        err.push_frame("first_function".to_string(), frame_span.clone());

        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first_function");
        assert_eq!(err.stack[0].definition_span, frame_span);

        // Push a second frame
        let frame2_span = test_span(10, 3, 10, 15);
        err.push_frame("second_function".to_string(), frame2_span.clone());

        assert_eq!(err.stack.len(), 2);
        assert_eq!(err.stack[1].label, "second_function");
        assert_eq!(err.stack[1].definition_span, frame2_span);
    }

    #[test]
    fn test_is_catchable() {
        // ResourceLimitExceeded is NOT catchable
        assert!(!ErrorKind::ResourceLimitExceeded {
            message: "test".to_string(),
        }
        .is_catchable());

        // All other (error_kind_variant_count() - 2) variants ARE catchable
        assert!(ErrorKind::KeyNotFound {
            key: "foo".to_string(),
            available_keys: vec![],
        }
        .is_catchable());
        assert!(ErrorKind::UndefinedVariable {
            name: "x".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::TypeMismatch {
            context: None,
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::TypeAssertFailed {
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::ArityMismatch {
            expected: ArityBound::Exact(1),
            got: 2
        }
        .is_catchable());
        assert!(ErrorKind::MissingRequiredParam {
            param: "x".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::NamedArgConflict {
            param: "x".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::UnknownNamedArg {
            name: "x".to_string(),
            valid_params: vec![]
        }
        .is_catchable());
        assert!(ErrorKind::NamedArgRejected {
            builtin: "test".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::DuplicateKey {
            key: "x".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::DivisionByZero {
            op: "/".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::IntegerOverflow {
            op: "+".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN
        }
        .is_catchable());
        assert!(ErrorKind::EmptyCollection {
            op: "head".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::ValueNotSerializable {
            value_type: "Function".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::FloatOutOfRange {
            builtin: "floor".to_string(),
            value: 1e20
        }
        .is_catchable());
        assert!(ErrorKind::IncludeIoError {
            path: "test.llt".to_string(),
            detail: "no such file".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::IncludeCycle {
            path: "test.llt".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::IncludeFileTooLarge {
            path: "big.llt".to_string(),
            size: 100_000_000,
            limit: 10_000_000
        }
        .is_catchable());
        assert!(ErrorKind::IncludeHashMismatch {
            path: "x.llt".to_string(),
            expected: "blake3:abc".to_string(),
            actual: "blake3:def".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::IncludeHashRequired {
            path: "x.llt".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::ParseConversion {
            builtin: "to-int".to_string(),
            input: "abc".to_string(),
            target: "Int".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::UriParseError {
            detail: "error".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::CircularDependency {
            name: "x".to_string(),
            cycle_path: Vec::new(),
        }
        .is_catchable());
        assert!(ErrorKind::UserError {
            message: "test".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::Internal {
            message: "test".to_string()
        }
        .is_catchable());
    }

    #[test]
    fn test_float_not_finite_nan_equality() {
        // Two FloatNotFinite errors with NaN should compare as equal
        let err1 = ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN,
        };
        let err2 = ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN,
        };
        assert_eq!(err1, err2);

        // Different NaN bit patterns should also work
        let err3 = ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::INFINITY,
        };
        assert_ne!(err1, err3);

        // Same value, different builtin
        let err4 = ErrorKind::FloatNotFinite {
            builtin: "other".to_string(),
            value: f64::NAN,
        };
        assert_ne!(err1, err4);
    }

    // Exhaustiveness enforcement: Rust's #[deny(non_exhaustive_omitted_patterns)] only works
    // for enums from external crates. For same-crate ErrorKind, this test helper + the
    // self-equality assertion below enforce that every variant is covered in code(), Display,
    // is_catchable(), and is_cacheable().
    /// Centralized variant list for test coverage. Adding a new ErrorKind variant
    /// without updating this list will cause test failures (runtime, not compile-time).
    fn all_error_kind_variants() -> Vec<ErrorKind> {
        vec![
            ErrorKind::KeyNotFound {
                key: "x".to_string(),
                available_keys: vec![],
            },
            ErrorKind::UndefinedVariable {
                name: "x".to_string(),
            },
            ErrorKind::TypeMismatch {
                context: None,
                expected: "Int".to_string(),
                got: "String".to_string(),
            },
            ErrorKind::TypeAssertFailed {
                expected: "Int".to_string(),
                got: "String".to_string(),
            },
            ErrorKind::ArityMismatch {
                expected: ArityBound::Exact(1),
                got: 2,
            },
            ErrorKind::MissingRequiredParam {
                param: "x".to_string(),
            },
            ErrorKind::NamedArgConflict {
                param: "x".to_string(),
            },
            ErrorKind::UnknownNamedArg {
                name: "x".to_string(),
                valid_params: vec![],
            },
            ErrorKind::NamedArgRejected {
                builtin: "test".to_string(),
            },
            ErrorKind::DuplicateKey {
                key: "x".to_string(),
            },
            ErrorKind::DivisionByZero {
                op: "/".to_string(),
            },
            ErrorKind::IntegerOverflow {
                op: "+".to_string(),
            },
            ErrorKind::FloatNotFinite {
                builtin: "test".to_string(),
                value: f64::NAN,
            },
            ErrorKind::EmptyCollection {
                op: "head".to_string(),
            },
            ErrorKind::ValueNotSerializable {
                value_type: "Function".to_string(),
            },
            ErrorKind::FloatOutOfRange {
                builtin: "floor".to_string(),
                value: 1e20,
            },
            ErrorKind::ResourceLimitExceeded {
                message: "test: resource limit exceeded (1000)".to_string(),
            },
            ErrorKind::CapabilityRequired {
                message: "test: capability required".to_string(),
            },
            ErrorKind::IncludeIoError {
                path: "x".to_string(),
                detail: "error".to_string(),
            },
            ErrorKind::IncludeCycle {
                path: "x".to_string(),
            },
            ErrorKind::IncludeFileTooLarge {
                path: "x".to_string(),
                size: 1000,
                limit: 100,
            },
            ErrorKind::IncludeHashMismatch {
                path: "x".to_string(),
                expected: "blake3:abc".to_string(),
                actual: "blake3:def".to_string(),
            },
            ErrorKind::IncludeHashRequired {
                path: "x".to_string(),
            },
            ErrorKind::ParseConversion {
                builtin: "to-int".to_string(),
                input: "x".to_string(),
                target: "Int".to_string(),
            },
            ErrorKind::UriParseError {
                detail: "error".to_string(),
            },
            ErrorKind::CircularDependency {
                name: "x".to_string(),
                cycle_path: Vec::new(),
            },
            ErrorKind::MatchExhaustion {
                scrutinee_type: "Int".to_string(),
            },
            ErrorKind::DuplicateVariable {
                name: "x".to_string(),
            },
            ErrorKind::UserError {
                message: "test".to_string(),
            },
            ErrorKind::Unimplemented {
                message: "...".to_string(),
            },
            ErrorKind::BuilderFinished {
                op: "set".to_string(),
            },
            ErrorKind::NoInstance {
                class_name: "Eq".to_string(),
                type_tags: vec!["Function".to_string()],
            },
            ErrorKind::MacroError {
                message: "bad splice".to_string(),
            },
            ErrorKind::SchemaViolation {
                violations: vec![("field".to_string(), "error".to_string())],
            },
            ErrorKind::KindMismatch {
                expected: "Type".to_string(),
                got: "Type → Type".to_string(),
            },
            ErrorKind::Internal {
                message: "test".to_string(),
            },
        ]
    }

    #[test]
    fn test_partialeq_all_variants_covered() {
        // Verify that all ErrorKind variants are covered by the PartialEq impl
        // by checking that each variant equals itself. This ensures the match
        // is exhaustive and doesn't rely solely on the catch-all `_ => false` arm.
        //
        // If a new variant is added without updating PartialEq, this test will
        // fail (since the catch-all would incorrectly return false for self-comparison).

        let variants = all_error_kind_variants();

        // Each variant should equal itself
        for variant in &variants {
            assert_eq!(
                variant, variant,
                "Variant {:?} does not equal itself",
                variant
            );
        }
    }

    #[test]
    fn test_error_code_exhaustiveness() {
        // Ensures every ErrorKind variant returns a valid E-code (not the catch-all E099,
        // unless it IS the Internal variant). This test prevents new variants from silently
        // falling through to E099 without an explicit code assignment.
        let variants = all_error_kind_variants();

        for variant in &variants {
            let code = variant.code();

            // All codes must match E\d{3} format
            assert!(
                code.len() == 4 && code.starts_with('E'),
                "ErrorKind variant {:?} has invalid code format: {}",
                variant,
                code
            );

            // Verify it's a valid 3-digit number after the E
            let number_part = &code[1..];
            assert!(
                number_part.parse::<u32>().is_ok(),
                "ErrorKind variant {:?} has non-numeric code: {}",
                variant,
                code
            );

            // Only Internal variant should return E099
            match variant {
                ErrorKind::Internal { .. } => {
                    assert_eq!(
                        code, "E099",
                        "Internal variant should return E099, got {}",
                        code
                    );
                }
                _ => {
                    assert_ne!(
                        code, "E099",
                        "Non-Internal variant {:?} should not return catch-all code E099",
                        variant
                    );
                }
            }
        }
    }

    #[test]
    fn test_arity_bound_display() {
        // Test Display output for all ArityBound variants
        assert_eq!(format!("{}", ArityBound::Exact(1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Exact(2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 0)), "0 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Range(2, 2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 3)), "1 to 3 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 5)), "0 to 5 arguments");
    }

    #[test]
    fn test_is_cacheable() {
        // All ErrorKind variants ARE cacheable (can be stored in Failed thunk state).
        // ResourceLimitExceeded IS cacheable (resource limits are absolute,
        // not context-dependent — a failed resource limit check
        // will fail consistently regardless of when it's retried).
        assert!(ErrorKind::KeyNotFound {
            key: "foo".to_string(),
            available_keys: vec![],
        }
        .is_cacheable());
        assert!(ErrorKind::UndefinedVariable {
            name: "x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::TypeMismatch {
            context: None,
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::TypeAssertFailed {
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::ArityMismatch {
            expected: ArityBound::Exact(1),
            got: 2
        }
        .is_cacheable());
        assert!(ErrorKind::MissingRequiredParam {
            param: "x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::NamedArgConflict {
            param: "x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::UnknownNamedArg {
            name: "x".to_string(),
            valid_params: vec![]
        }
        .is_cacheable());
        assert!(ErrorKind::NamedArgRejected {
            builtin: "test".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::DuplicateKey {
            key: "x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::DivisionByZero {
            op: "/".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::IntegerOverflow {
            op: "+".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN
        }
        .is_cacheable());
        assert!(ErrorKind::EmptyCollection {
            op: "head".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::ValueNotSerializable {
            value_type: "Function".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::FloatOutOfRange {
            builtin: "floor".to_string(),
            value: 1e20
        }
        .is_cacheable());
        assert!(ErrorKind::ResourceLimitExceeded {
            message: "test".to_string(),
        }
        .is_cacheable());
        assert!(ErrorKind::IncludeIoError {
            path: "test.llt".to_string(),
            detail: "no such file".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::IncludeCycle {
            path: "test.llt".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::IncludeFileTooLarge {
            path: "big.llt".to_string(),
            size: 100_000_000,
            limit: 10_000_000
        }
        .is_cacheable());
        assert!(ErrorKind::IncludeHashMismatch {
            path: "x.llt".to_string(),
            expected: "blake3:abc".to_string(),
            actual: "blake3:def".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::IncludeHashRequired {
            path: "x.llt".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::ParseConversion {
            builtin: "to-int".to_string(),
            input: "abc".to_string(),
            target: "Int".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::UriParseError {
            detail: "error".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::CircularDependency {
            name: "x".to_string(),
            cycle_path: Vec::new(),
        }
        .is_cacheable());
        assert!(ErrorKind::MatchExhaustion {
            scrutinee_type: "Int".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::DuplicateVariable {
            name: "x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::UserError {
            message: "test".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::Internal {
            message: "test".to_string()
        }
        .is_cacheable());
    }

    #[test]
    fn test_error_kind_display_all_variants() {
        // Verify Display output for ALL ErrorKind variants to prevent
        // message quality regressions and ensure error messages are helpful.

        // Access errors (E000-E009)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::KeyNotFound {
                    key: "name".to_string(),
                    available_keys: vec![],
                }
            ),
            "key not found: name"
        );
        // UndefinedVariable with likely string literal triggers hint
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UndefinedVariable {
                    name: "x".to_string()
                }
            ),
            "undefined variable: x (did you mean the string \"x\"? Use quotes.)"
        );
        // UndefinedVariable with % prefix does not trigger hint
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UndefinedVariable {
                    name: "%foo".to_string()
                }
            ),
            "undefined variable: %foo"
        );
        // UndefinedVariable with known builtin does not trigger hint
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UndefinedVariable {
                    name: "map".to_string()
                }
            ),
            "undefined variable: map"
        );

        // Type errors (E010-E019)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::TypeMismatch {
                    context: None,
                    expected: "Int".to_string(),
                    got: "String".to_string()
                }
            ),
            "type mismatch: expected Int, got String"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::TypeMismatch {
                    context: Some("merge".to_string()),
                    expected: "Dict".to_string(),
                    got: "Int".to_string()
                }
            ),
            "merge: expected Dict, got Int"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::TypeAssertFailed {
                    expected: "Int".to_string(),
                    got: "String".to_string()
                }
            ),
            "type assertion failed: expected Int, got String"
        );

        // Call errors (E020-E029)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ArityMismatch {
                    expected: ArityBound::Exact(1),
                    got: 0
                }
            ),
            "arity mismatch: expected 1 argument, got 0"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ArityMismatch {
                    expected: ArityBound::Exact(2),
                    got: 3
                }
            ),
            "arity mismatch: expected 2 arguments, got 3"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ArityMismatch {
                    expected: ArityBound::Range(1, 3),
                    got: 5
                }
            ),
            "arity mismatch: expected 1 to 3 arguments, got 5"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::MissingRequiredParam {
                    param: "name".to_string()
                }
            ),
            "missing argument for required parameter 'name'"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::NamedArgConflict {
                    param: "x".to_string()
                }
            ),
            "parameter 'x' received both positional and named argument"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UnknownNamedArg {
                    name: "foo".to_string(),
                    valid_params: vec!["x".to_string(), "y".to_string()],
                }
            ),
            "unexpected named argument: foo (valid parameter names: x, y)"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UnknownNamedArg {
                    name: "foo".to_string(),
                    valid_params: vec![],
                }
            ),
            "unexpected named argument: foo (function has no parameters)"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::NamedArgRejected {
                    builtin: "map".to_string()
                }
            ),
            "map does not accept named arguments"
        );

        // Value errors (E030-E039)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::DuplicateKey {
                    key: "name".to_string()
                }
            ),
            "duplicate key: name"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::DivisionByZero {
                    op: "/".to_string()
                }
            ),
            "/: division by zero"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IntegerOverflow {
                    op: "+".to_string()
                }
            ),
            "+: integer overflow"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::FloatNotFinite {
                    builtin: "floor".to_string(),
                    value: f64::NAN
                }
            ),
            "floor: NaN is not a finite number"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::FloatNotFinite {
                    builtin: "round".to_string(),
                    value: f64::INFINITY
                }
            ),
            "round: inf is not a finite number"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::EmptyCollection {
                    op: "head".to_string()
                }
            ),
            "head on empty collection"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ValueNotSerializable {
                    value_type: "Function".to_string()
                }
            ),
            "cannot serialize Function to JSON"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ValueNotSerializable {
                    value_type: "Proxy".to_string()
                }
            ),
            "cannot serialize Proxy to JSON"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::FloatOutOfRange {
                    builtin: "floor".to_string(),
                    value: 1e20
                }
            ),
            "floor: 100000000000000000000 is out of range for Int"
        );

        // Limit errors (E040-E049)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ResourceLimitExceeded {
                    message: "upper: output would exceed 64 MB limit (67108864 bytes)".to_string(),
                }
            ),
            "upper: output would exceed 64 MB limit (67108864 bytes)"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::CapabilityRequired {
                    message: "%net@NetCap is required but not provided".to_string(),
                }
            ),
            "%net@NetCap is required but not provided"
        );

        // Include errors (E050-E059)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IncludeIoError {
                    path: "config.llt".to_string(),
                    detail: "No such file or directory".to_string()
                }
            ),
            "include: cannot access \"config.llt\": No such file or directory"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IncludeCycle {
                    path: "a.llt".to_string()
                }
            ),
            "circular include detected: \"a.llt\""
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IncludeFileTooLarge {
                    path: "huge.llt".to_string(),
                    size: 2_000_000,
                    limit: 1_000_000
                }
            ),
            "include: file \"huge.llt\" is 2000000 bytes, exceeds 1000000 byte limit"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IncludeHashMismatch {
                    path: "config.llt".to_string(),
                    expected: "blake3:abc123".to_string(),
                    actual: "blake3:def456".to_string()
                }
            ),
            "include: integrity check failed for \"config.llt\": expected blake3:abc123, got blake3:def456"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::IncludeHashRequired {
                    path: "config.llt".to_string()
                }
            ),
            "include: integrity hash required for \"config.llt\" (--require-integrity)"
        );

        // Conversion errors (E060-E069)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ParseConversion {
                    builtin: "to-int".to_string(),
                    input: "abc".to_string(),
                    target: "Int".to_string()
                }
            ),
            "to-int: cannot parse \"abc\" as Int"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UriParseError {
                    detail: "missing scheme: example.com".to_string()
                }
            ),
            "URI parse error: missing scheme: example.com"
        );

        // Evaluation structure (E070-E079)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::CircularDependency {
                    name: "x".to_string(),
                    cycle_path: Vec::new(),
                }
            ),
            "circular dependency detected while evaluating x"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::MatchExhaustion {
                    scrutinee_type: "Int".to_string()
                }
            ),
            "non-exhaustive match: no pattern matched the Int value"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::DuplicateVariable {
                    name: "x".to_string()
                }
            ),
            "duplicate variable in pattern: 'x' appears more than once"
        );

        // User-generated (E080-E089)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UserError {
                    message: "invalid config".to_string()
                }
            ),
            "invalid config"
        );

        // Schema validation (E090-E094)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::KindMismatch {
                    expected: "Type".to_string(),
                    got: "Type → Type".to_string(),
                }
            ),
            "kind mismatch: expected Type, got Type → Type"
        );

        // Escape hatch (E095-E099)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::Internal {
                    message: "unexpected state".to_string()
                }
            ),
            "unexpected state"
        );
    }

    #[test]
    fn test_error_kind_code_exhaustiveness() {
        // Verify that ALL ErrorKind variants have error codes and that
        // all codes are unique. This catches silent breakage when new
        // variants are added without updating code().

        let variants = all_error_kind_variants();

        let mut codes = std::collections::HashSet::new();

        for variant in &variants {
            let code = variant.code();

            // Each code should start with "E" followed by digits
            assert!(
                code.starts_with('E'),
                "Error code {:?} for variant {:?} does not start with 'E'",
                code,
                variant
            );
            assert!(
                code.len() > 1,
                "Error code {:?} for variant {:?} has no digits",
                code,
                variant
            );
            let digits = &code[1..];
            assert!(
                digits.chars().all(|c| c.is_ascii_digit()),
                "Error code {:?} for variant {:?} contains non-digit characters after 'E'",
                code,
                variant
            );

            // Each code should be unique
            assert!(
                codes.insert(code),
                "Duplicate error code {:?} found for variant {:?}",
                code,
                variant
            );
        }
    }

    #[test]
    fn test_stack_frame_accumulation_chain() {
        // Test that stack frames accumulate correctly through nested materialization
        let def_span = test_span(1, 1, 1, 10);
        let mat_span = test_span(5, 1, 5, 10);
        let frame1_span = test_span(10, 1, 10, 15);
        let frame2_span = test_span(15, 1, 15, 20);
        let frame3_span = test_span(20, 1, 20, 25);

        // Simulate error propagating through dict -> thunk -> builtin chain
        let err = EvalError::type_mismatch("Int", "String", def_span.clone())
            .with_materialization_span(mat_span.clone())
            .with_frame("dict entry 'inner'".to_string(), frame1_span.clone())
            .with_frame("dict entry 'outer'".to_string(), frame2_span.clone())
            .with_frame("materialized".to_string(), frame3_span.clone());

        assert_eq!(err.definition_span, def_span);
        assert_eq!(err.materialization_span, Some(mat_span));
        assert_eq!(err.stack.len(), 3);
        assert_eq!(err.stack[0].label, "dict entry 'inner'");
        assert_eq!(err.stack[0].definition_span, frame1_span);
        assert_eq!(err.stack[1].label, "dict entry 'outer'");
        assert_eq!(err.stack[1].definition_span, frame2_span);
        assert_eq!(err.stack[2].label, "materialized");
        assert_eq!(err.stack[2].definition_span, frame3_span);
    }

    #[test]
    fn test_stack_frame_display_multi_level() {
        // Test that display output includes all stack frames
        let def_span = test_span(1, 1, 1, 10);
        let mat_span = test_span(5, 1, 5, 10);
        let frame1_span = test_span(10, 1, 10, 15);
        let frame2_span = test_span(15, 1, 15, 20);

        let err = EvalError::key_not_found("missing_key", vec![], def_span)
            .with_materialization_span(mat_span)
            .with_frame("dict entry 'a'".to_string(), frame1_span)
            .with_frame("dict entry 'b'".to_string(), frame2_span);

        let display = format!("{err}");

        // Should contain error code
        assert!(display.contains("[E001]"));
        // Should contain error message
        assert!(display.contains("key not found: missing_key"));
        // Should contain definition span
        assert!(display.contains("defined at 1:1-1:10"));
        // Should contain materialization span
        assert!(display.contains("materialized at 5:1-5:10"));
        // Should contain all stack frames
        assert!(display.contains("in dict entry 'a' at 10:1-10:15"));
        assert!(display.contains("in dict entry 'b' at 15:1-15:20"));
    }

    #[test]
    fn test_stack_frame_preserves_original_materialization_span() {
        // Test that when multiple access sites trigger the same error,
        // the original materialization span is preserved
        let def_span = test_span(1, 1, 1, 10);
        let first_mat_span = test_span(5, 1, 5, 10);
        let second_access_span = test_span(8, 1, 8, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span)
            .with_materialization_span(first_mat_span.clone());

        // Simulate a second access from a different location
        // Should preserve original mat_span and add second access as stack frame
        assert_eq!(err.materialization_span, Some(first_mat_span.clone()));

        // Manually simulate what attach_materialization_context does
        if !err
            .stack
            .iter()
            .any(|f| f.definition_span == second_access_span)
        {
            err.push_frame("materialized".to_string(), second_access_span.clone());
        }

        assert_eq!(err.materialization_span, Some(first_mat_span));
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].definition_span, second_access_span);
    }

    #[test]
    fn test_stack_frame_avoids_duplicates() {
        // Test that duplicate stack frames (same span) are not added
        let def_span = test_span(1, 1, 1, 10);
        let frame_span = test_span(5, 1, 5, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span);

        err.push_frame("first".to_string(), frame_span.clone());
        assert_eq!(err.stack.len(), 1);

        // Manually check for duplicate before adding (this is what attach_materialization_context does)
        if !err.stack.iter().any(|f| f.definition_span == frame_span) {
            err.push_frame("second".to_string(), frame_span);
        }

        // Should still be 1 frame (duplicate was avoided)
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first");
    }

    #[test]
    fn test_resource_limit_exceeded_not_catchable() {
        // Verify that ResourceLimitExceeded is non-catchable
        let err = ErrorKind::ResourceLimitExceeded {
            message: "collect: exceeded maximum collection size (1000000)".to_string(),
        };
        assert!(
            !err.is_catchable(),
            "ResourceLimitExceeded must not be catchable by try"
        );
    }

    #[test]
    fn test_resource_limit_exceeded_is_cacheable() {
        // ResourceLimitExceeded IS cacheable
        // (resource limits are absolute, not context-dependent)
        let err = ErrorKind::ResourceLimitExceeded {
            message: "upper: output would exceed 64 MB limit".to_string(),
        };
        assert!(
            err.is_cacheable(),
            "ResourceLimitExceeded should be cacheable"
        );
    }

    #[test]
    fn test_resource_limit_exceeded_display() {
        let err = EvalError::resource_limit_exceeded(
            "collect: exceeded maximum collection size (1000000)".to_string(),
            test_span(5, 10, 5, 20),
        );
        let display = format!("{err}");

        // Should contain error code E043
        assert!(display.contains("[E043]"));
        // Should contain the full message
        assert!(display.contains("collect: exceeded maximum collection size"));
        // Should contain the limit value
        assert!(display.contains("1000000"));
    }

    #[test]
    fn test_origin_span_frames_filtered_from_display() {
        // Verify that stack frames with Span::origin() (synthetic stdlib/builtin frames)
        // are NOT shown in error display output
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let real_frame_span = test_span(10, 2, 10, 8);

        let mut err = EvalError::internal("bad value".to_string(), def_span)
            .with_materialization_span(mat_span);

        // Add a real user frame
        err.push_frame("user_function".to_string(), real_frame_span);

        // Add a synthetic origin frame (should be filtered out)
        err.push_frame("stdlib_internal".to_string(), Span::origin());

        // Add another real frame
        let real_frame2_span = test_span(15, 1, 15, 12);
        err.push_frame("another_user_function".to_string(), real_frame2_span);

        let display = format!("{err}");

        // Should contain the real frames
        assert!(display.contains("in user_function at 10:2-10:8"));
        assert!(display.contains("in another_user_function at 15:1-15:12"));

        // Should NOT contain the origin frame (1:1-1:1).
        // Note: Span::origin() uses exact structural equality — offset=0, line=1, col=1
        // for both start and end.  Real user code at line 1 col 1 would NOT be filtered
        // because it would have a non-zero byte offset, making the spans structurally
        // distinct.  Only the synthetic Span::origin() sentinel (offset 0, empty range)
        // triggers the filter at error.rs:829.
        assert!(!display.contains("stdlib_internal"));
        assert!(!display.contains("1:1-1:1"));
    }

    #[test]
    fn test_error_code_prefix_format() {
        // Verify that error codes follow the [EXXX] format exactly
        let err = EvalError::key_not_found("test", vec![], test_span(1, 1, 1, 5));
        let display = format!("{err}");

        // Should start with [E001]
        assert!(display.starts_with("[E001]"));

        // Verify all error codes follow the pattern
        let test_cases = vec![
            (
                EvalError::key_not_found("x", vec![], test_span(1, 1, 1, 5)),
                "[E001]",
            ),
            (
                EvalError {
                    kind: ErrorKind::UndefinedVariable {
                        name: "x".to_string(),
                    },
                    definition_span: test_span(1, 1, 1, 5),
                    materialization_span: None,
                    stack: SmallVec::new(),
                    secondary_span: None,
                    macro_expansion: None,
                    blame: None,
                    pipeline_stage: None,
                },
                "[E002]",
            ),
            (
                EvalError::type_mismatch("Int", "String", test_span(1, 1, 1, 5)),
                "[E010]",
            ),
            (
                EvalError::arity_mismatch(1, 2, test_span(1, 1, 1, 5)),
                "[E020]",
            ),
            (
                EvalError::circular_dependency("x", test_span(1, 1, 1, 5), Vec::new()),
                "[E070]",
            ),
            (
                EvalError::user_error("test".to_string(), test_span(1, 1, 1, 5)),
                "[E080]",
            ),
        ];

        for (err, expected_prefix) in test_cases {
            let display = format!("{err}");
            assert!(
                display.starts_with(expected_prefix),
                "Expected {}, got: {}",
                expected_prefix,
                display
            );
        }
    }

    #[test]
    fn test_key_not_found_with_suggestion() {
        // Test close match suggestion (typo: nme → name)
        let err = ErrorKind::KeyNotFound {
            key: "nme".to_string(),
            available_keys: vec!["name".to_string(), "age".to_string()],
        };
        let display = format!("{err}");
        assert!(display.contains("key not found: nme"));
        assert!(display.contains("did you mean: 'name'"));
    }

    #[test]
    fn test_key_not_found_with_available_keys() {
        // Test no close match - should show available keys
        let err = ErrorKind::KeyNotFound {
            key: "xyz".to_string(),
            available_keys: vec!["name".to_string(), "age".to_string(), "address".to_string()],
        };
        let display = format!("{err}");
        assert!(display.contains("key not found: xyz"));
        assert!(display.contains("available keys:"));
        assert!(display.contains("name"));
        assert!(display.contains("age"));
        assert!(display.contains("address"));
    }

    #[test]
    fn test_key_not_found_truncates_long_list() {
        // Test that long key lists are truncated to 5 items
        let err = ErrorKind::KeyNotFound {
            key: "missing".to_string(),
            available_keys: vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f".to_string(),
                "g".to_string(),
            ],
        };
        let display = format!("{err}");
        assert!(display.contains("available keys:"));
        assert!(display.contains("..."));
        // Should show first 5 keys as a joined list and then "..." — verify both
        // the exact joined sequence and that the truncated keys are absent
        assert!(display.contains("a, b, c, d, e"));
        assert!(!display.contains(", f"));
    }

    #[test]
    fn test_key_not_found_empty_available_keys() {
        // Test empty dict - no suggestions
        let err = ErrorKind::KeyNotFound {
            key: "foo".to_string(),
            available_keys: vec![],
        };
        let display = format!("{err}");
        assert_eq!(display, "key not found: foo");
        assert!(!display.contains("did you mean"));
        assert!(!display.contains("available keys"));
    }

    #[test]
    fn test_stdlib_internal_frames_all_shown_in_display() {
        // All frames with real source locations are shown — no suffix-based filtering.
        // Formerly, -impl/-step/-check/-merge frames were hidden; now they are all visible.
        let def_span = test_span(5, 1, 5, 10);
        let mat_span = test_span(10, 1, 10, 5);

        let mut err = EvalError::internal("error in stdlib".to_string(), def_span)
            .with_materialization_span(mat_span);

        // Add user-facing stdlib function
        err.push_frame("[map ...]".to_string(), test_span(8, 1, 8, 10));

        // Add internal helper frames — these must now also be visible
        err.push_frame("[map-impl ...]".to_string(), test_span(100, 1, 100, 10));
        err.push_frame("[remove-step ...]".to_string(), test_span(200, 1, 200, 10));
        err.push_frame("[cond-check ...]".to_string(), test_span(300, 1, 300, 10));

        // Add another user-facing function
        err.push_frame("[filter ...]".to_string(), test_span(12, 1, 12, 15));

        let display = format!("{err}");

        // All frames with real spans must appear
        assert!(display.contains("in [map ...] at 8:1-8:10"));
        assert!(display.contains("in [filter ...] at 12:1-12:15"));
        assert!(display.contains("map-impl"));
        assert!(display.contains("remove-step"));
        assert!(display.contains("cond-check"));
        assert!(display.contains("100:1-100:10"));
        assert!(display.contains("200:1-200:10"));
        assert!(display.contains("300:1-300:10"));
    }

    #[test]
    fn test_stdlib_frame_substring_not_filtered() {
        // All frames with real spans are visible — no suffix filtering at all.
        let def_span = test_span(1, 1, 1, 5);
        let err = EvalError::internal("test".to_string(), def_span).with_frame(
            "[multi-step-validator ...]".to_string(),
            test_span(10, 1, 10, 5),
        );
        let display = format!("{err}");
        assert!(
            display.contains("multi-step-validator"),
            "all frames with real spans must appear; got: {display}"
        );
    }

    #[test]
    fn test_should_display_frame_all_real_spans_visible() {
        // All frames with real spans must return true, regardless of label suffix.
        let real_span = test_span(5, 1, 5, 10);
        let impl_frame = StackFrame {
            label: "[map-impl ...]".to_string(),
            definition_span: real_span.clone(),
            materialization_span: real_span.clone(),
        };
        assert!(
            should_display_frame(&impl_frame),
            "frame with -impl suffix and real span must return true"
        );
        let step_frame = StackFrame {
            label: "[remove-step ...]".to_string(),
            definition_span: real_span.clone(),
            materialization_span: real_span.clone(),
        };
        assert!(
            should_display_frame(&step_frame),
            "frame with -step suffix and real span must return true"
        );
        let check_frame = StackFrame {
            label: "[validate-check ...]".to_string(),
            definition_span: real_span.clone(),
            materialization_span: real_span.clone(),
        };
        assert!(
            should_display_frame(&check_frame),
            "frame with -check suffix and real span must return true"
        );
        let merge_frame = StackFrame {
            label: "[sort-merge ...]".to_string(),
            definition_span: real_span.clone(),
            materialization_span: real_span.clone(),
        };
        assert!(
            should_display_frame(&merge_frame),
            "frame with -merge suffix and real span must return true"
        );
        let user_frame = StackFrame {
            label: "[map ...]".to_string(),
            definition_span: real_span.clone(),
            materialization_span: real_span,
        };
        assert!(
            should_display_frame(&user_frame),
            "user-facing frame with real span must return true"
        );
    }

    #[test]
    fn test_should_display_frame_origin_span() {
        // A frame whose span is Span::origin() (synthetic stdlib/builtin location)
        // must not be displayed regardless of its label.
        let origin_frame = StackFrame {
            label: "[builtin-fn ...]".to_string(),
            definition_span: Span::origin(),
            materialization_span: Span::origin(),
        };
        assert!(
            !should_display_frame(&origin_frame),
            "frame with Span::origin() must return false"
        );
        // A frame with a real span and a non-hidden label must be displayed.
        let real_frame = StackFrame {
            label: "[user-fn ...]".to_string(),
            definition_span: test_span(3, 1, 3, 10),
            materialization_span: test_span(3, 1, 3, 10),
        };
        assert!(
            should_display_frame(&real_frame),
            "frame with real span and non-hidden label must return true"
        );
    }

    #[test]
    fn test_infer_materialization_verb_skips_origin_span() {
        // Stack where the first frame has Span::origin() (skipped) and the second
        // has a real "[X ...]" label — verb must be "called at", not "materialized at".
        let frames = vec![
            StackFrame {
                label: "stdlib-internal".to_string(),
                definition_span: Span::origin(),
                materialization_span: Span::origin(),
            },
            StackFrame {
                label: "[user-fn ...]".to_string(),
                definition_span: test_span(7, 1, 7, 20),
                materialization_span: test_span(7, 1, 7, 20),
            },
        ];
        assert_eq!(
            infer_materialization_verb(&frames),
            "called at",
            "origin-span frame must be skipped; second frame's '[...]' label must drive the verb"
        );
    }

    // -----------------------------------------------------------------------
    // Constructor-level unit tests: one test per EvalError named constructor
    // that was not already covered by test_eval_error_* tests above.
    // These complement the exhaustiveness checks in all_error_kind_variants()
    // and test_error_kind_display_all_variants() by exercising each constructor
    // directly and verifying the resulting kind, message, code, and span.
    // -----------------------------------------------------------------------

    #[test]
    fn test_eval_error_user_error_constructor() {
        let span = test_span(2, 3, 2, 15);
        let err = EvalError::user_error("custom error message".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::UserError { .. }));
        assert_eq!(err.kind.code(), "E080");
        assert_eq!(err.kind.to_string(), "custom error message");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_named_arg_rejected_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::named_arg_rejected("floor".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::NamedArgRejected { .. }));
        assert_eq!(err.kind.code(), "E023");
        assert_eq!(
            err.kind.to_string(),
            "floor does not accept named arguments"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_integer_overflow_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::integer_overflow("*".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::IntegerOverflow { .. }));
        assert_eq!(err.kind.code(), "E032");
        assert_eq!(err.kind.to_string(), "*: integer overflow");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_type_mismatch_ctx_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err =
            EvalError::type_mismatch_ctx("document pipeline".to_string(), "Dict", "Int", span);
        assert!(matches!(
            err.kind,
            ErrorKind::TypeMismatch {
                context: Some(_),
                ..
            }
        ));
        assert_eq!(err.kind.code(), "E010");
        assert_eq!(
            err.kind.to_string(),
            "document pipeline: expected Dict, got Int"
        );
        assert!(err.kind.is_catchable());
    }

    #[test]
    fn test_eval_error_division_by_zero_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::division_by_zero("%".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::DivisionByZero { .. }));
        assert_eq!(err.kind.code(), "E031");
        assert_eq!(err.kind.to_string(), "%: division by zero");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_float_not_finite_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::float_not_finite("round".to_string(), f64::INFINITY, span);
        assert!(matches!(err.kind, ErrorKind::FloatNotFinite { .. }));
        assert_eq!(err.kind.code(), "E033");
        assert!(err.kind.to_string().contains("not a finite number"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_empty_collection_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::empty_collection("tail".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::EmptyCollection { .. }));
        assert_eq!(err.kind.code(), "E034");
        assert_eq!(err.kind.to_string(), "tail on empty collection");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_value_not_serializable_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::value_not_serializable("Builtin".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::ValueNotSerializable { .. }));
        assert_eq!(err.kind.code(), "E035");
        assert_eq!(err.kind.to_string(), "cannot serialize Builtin to JSON");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_float_out_of_range_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::float_out_of_range("to-int".to_string(), 1e300, span);
        assert!(matches!(err.kind, ErrorKind::FloatOutOfRange { .. }));
        assert_eq!(err.kind.code(), "E036");
        assert!(err.kind.to_string().contains("out of range for Int"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_undefined_variable_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::undefined_variable("myvar".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::UndefinedVariable { .. }));
        assert_eq!(err.kind.code(), "E002");
        // "myvar" is all lowercase/alphanumeric and not a builtin, so triggers hint
        assert_eq!(
            err.kind.to_string(),
            "undefined variable: myvar (did you mean the string \"myvar\"? Use quotes.)"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_type_assert_failed_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::type_assert_failed("Bool", "String", span);
        assert!(matches!(err.kind, ErrorKind::TypeAssertFailed { .. }));
        assert_eq!(err.kind.code(), "E011");
        assert_eq!(
            err.kind.to_string(),
            "type assertion failed: expected Bool, got String"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_named_arg_conflict_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::named_arg_conflict("separator".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::NamedArgConflict { .. }));
        assert_eq!(err.kind.code(), "E021");
        assert_eq!(
            err.kind.to_string(),
            "parameter 'separator' received both positional and named argument"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_unknown_named_arg_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::unknown_named_arg(
            "typo".to_string(),
            vec!["sep".to_string(), "limit".to_string()],
            span,
        );
        assert!(matches!(err.kind, ErrorKind::UnknownNamedArg { .. }));
        assert_eq!(err.kind.code(), "E022");
        assert!(err
            .kind
            .to_string()
            .contains("unexpected named argument: typo"));
        assert!(err.kind.to_string().contains("sep"));
        assert!(err.kind.to_string().contains("limit"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_duplicate_key_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::duplicate_key("host", span);
        assert!(matches!(err.kind, ErrorKind::DuplicateKey { .. }));
        assert_eq!(err.kind.code(), "E030");
        assert_eq!(err.kind.to_string(), "duplicate key: host");
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_resource_limit_exceeded_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::resource_limit_exceeded(
            "collect: exceeded maximum collection size (1000000)",
            span,
        );
        assert!(matches!(err.kind, ErrorKind::ResourceLimitExceeded { .. }));
        assert_eq!(err.kind.code(), "E043");
        assert!(err
            .kind
            .to_string()
            .contains("exceeded maximum collection size"));
        // ResourceLimitExceeded is NOT catchable (safety boundary)
        assert!(
            !err.kind.is_catchable(),
            "ResourceLimitExceeded must not be catchable"
        );
        // ResourceLimitExceeded IS cacheable (deterministic — always fails)
        assert!(
            err.kind.is_cacheable(),
            "ResourceLimitExceeded should be cacheable"
        );
    }

    #[test]
    fn test_eval_error_include_io_error_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::include_io_error(
            "missing.llt".to_string(),
            "No such file or directory".to_string(),
            span,
        );
        assert!(matches!(err.kind, ErrorKind::IncludeIoError { .. }));
        assert_eq!(err.kind.code(), "E051");
        assert!(err
            .kind
            .to_string()
            .contains("cannot access \"missing.llt\""));
        assert!(err.kind.to_string().contains("No such file or directory"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_include_cycle_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::include_cycle("recursive.llt".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::IncludeCycle { .. }));
        assert_eq!(err.kind.code(), "E052");
        assert!(err.kind.to_string().contains("circular include detected"));
        assert!(err.kind.to_string().contains("recursive.llt"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_include_file_too_large_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err =
            EvalError::include_file_too_large("huge.llt".to_string(), 20_000_000, 10_485_760, span);
        assert!(matches!(err.kind, ErrorKind::IncludeFileTooLarge { .. }));
        assert_eq!(err.kind.code(), "E054");
        assert!(err.kind.to_string().contains("huge.llt"));
        assert!(err.kind.to_string().contains("20000000"));
        assert!(err.kind.to_string().contains("10485760"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_include_hash_mismatch_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::include_hash_mismatch(
            "config.llt".to_string(),
            "blake3:aabbcc".to_string(),
            "blake3:112233".to_string(),
            span,
        );
        assert!(matches!(err.kind, ErrorKind::IncludeHashMismatch { .. }));
        assert_eq!(err.kind.code(), "E055");
        assert!(err.kind.to_string().contains("integrity check failed"));
        assert!(err.kind.to_string().contains("config.llt"));
        assert!(err.kind.to_string().contains("blake3:aabbcc"));
        assert!(err.kind.to_string().contains("blake3:112233"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_include_hash_required_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::include_hash_required("untrusted.llt".to_string(), span);
        assert!(matches!(err.kind, ErrorKind::IncludeHashRequired { .. }));
        assert_eq!(err.kind.code(), "E056");
        assert!(err.kind.to_string().contains("integrity hash required"));
        assert!(err.kind.to_string().contains("untrusted.llt"));
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_parse_conversion_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::parse_conversion(
            "to-float".to_string(),
            "not-a-number".to_string(),
            "Float",
            span,
        );
        assert!(matches!(err.kind, ErrorKind::ParseConversion { .. }));
        assert_eq!(err.kind.code(), "E060");
        assert_eq!(
            err.kind.to_string(),
            "to-float: cannot parse \"not-a-number\" as Float"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    #[test]
    fn test_eval_error_missing_required_param_constructor() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::missing_required_param("separator", span);
        assert!(matches!(err.kind, ErrorKind::MissingRequiredParam { .. }));
        assert_eq!(err.kind.code(), "E024");
        assert_eq!(
            err.kind.to_string(),
            "missing argument for required parameter 'separator'"
        );
        assert!(err.kind.is_catchable());
        assert!(err.kind.is_cacheable());
    }

    /// Verify error code uniqueness across all ErrorKind variants.
    /// Each variant must have a distinct error code — no two variants share a code.
    #[test]
    fn test_all_error_codes_are_unique_and_valid() {
        let variants = all_error_kind_variants();
        let mut seen_codes = std::collections::HashMap::new();
        for variant in &variants {
            let code = variant.code();
            // Must follow [E\d\d\d] format (3 digits after E)
            assert!(
                code.starts_with('E')
                    && code.len() == 4
                    && code[1..].chars().all(|c| c.is_ascii_digit()),
                "Error code {:?} for {:?} must follow E### format",
                code,
                variant
            );
            if let Some(prev) = seen_codes.insert(code, format!("{:?}", variant)) {
                panic!(
                    "Duplicate error code {:?} for variants: {} and {:?}",
                    code, prev, variant
                );
            }
        }
        // Verify the count matches all_error_kind_variants() — the canonical list.
        // If variants are added or removed, update all_error_kind_variants() to match.
        // Current count: 36 variants (DepthExceeded removed in B-314, absorbed into ResourceLimitExceeded).
        assert_eq!(
            variants.len(),
            36,
            "Expected 36 ErrorKind variants in all_error_kind_variants(); got {}. \
             Update all_error_kind_variants() if variants were added or removed.",
            variants.len()
        );
    }

    // ── render_span_snippet tests ──────────────────────────────────────────

    #[test]
    fn test_render_span_snippet_single_line() {
        let source = "line1\nlet x = 42\nline3";
        let span = Span {
            start: crate::ast::Position {
                offset: 6,
                line: 2,
                column: 1,
            },
            end: crate::ast::Position {
                offset: 9,
                line: 2,
                column: 4,
            },
            file: None,
        };
        let snippet = render_span_snippet(source, span).unwrap();

        // Should show line 2 with "let" underlined
        assert!(snippet.contains("2 | let x = 42"));
        assert!(snippet.contains("  | ^^^"));
        assert!(!snippet.contains("...")); // single-line span, no continuation marker
    }

    #[test]
    fn test_render_span_snippet_origin_suppressed() {
        let source = "test source";
        let snippet = render_span_snippet(source, Span::origin());
        assert_eq!(snippet, None); // Span::origin() is 0:0-0:0, should return None
    }

    #[test]
    fn test_render_span_snippet_multiline() {
        // source:
        //   line 1: "line1"
        //   line 2: "let x = ["
        //   line 3: "  42"
        //   line 4: "]"
        //   line 5: "line5"
        //
        // Span covers lines 2-4, col 1 to col 2 (i.e., "let x = [\n  42\n]")
        let source = "line1\nlet x = [\n  42\n]\nline5";
        let span = Span {
            start: crate::ast::Position {
                offset: 6,
                line: 2,
                column: 1,
            },
            end: crate::ast::Position {
                offset: 21,
                line: 4,
                column: 2,
            },
            file: None,
        };
        let snippet = render_span_snippet(source, span).unwrap();

        // All three lines must appear in the snippet.
        assert!(
            snippet.contains("2 | let x = ["),
            "missing first line: {snippet}"
        );
        assert!(
            snippet.contains("3 |   42"),
            "missing middle line: {snippet}"
        );
        assert!(snippet.contains("4 | ]"), "missing last line: {snippet}");

        // Caret line must appear (at least one "^").
        assert!(snippet.contains('^'), "missing caret: {snippet}");

        // Old "..." marker must NOT appear — we now show all lines.
        assert!(
            !snippet.contains("..."),
            "unexpected '...' in snippet: {snippet}"
        );
    }

    #[test]
    fn test_error_kind_code_compile_time_exhaustive() {
        // This exhaustive match ensures every ErrorKind variant is covered by code().
        // Adding a new ErrorKind variant without updating code() will cause a compile error here.
        fn assert_code_exhaustive(kind: &ErrorKind) -> &'static str {
            match kind {
                ErrorKind::KeyNotFound { .. } => "E001",
                ErrorKind::UndefinedVariable { .. } => "E002",
                ErrorKind::TypeMismatch { .. } => "E010",
                ErrorKind::TypeAssertFailed { .. } => "E011",
                ErrorKind::NoInstance { .. } => "E013",
                ErrorKind::MacroError { .. } => "E012",
                ErrorKind::ArityMismatch { .. } => "E020",
                ErrorKind::NamedArgConflict { .. } => "E021",
                ErrorKind::UnknownNamedArg { .. } => "E022",
                ErrorKind::NamedArgRejected { .. } => "E023",
                ErrorKind::MissingRequiredParam { .. } => "E024",
                ErrorKind::DuplicateKey { .. } => "E030",
                ErrorKind::DivisionByZero { .. } => "E031",
                ErrorKind::IntegerOverflow { .. } => "E032",
                ErrorKind::FloatNotFinite { .. } => "E033",
                ErrorKind::EmptyCollection { .. } => "E034",
                ErrorKind::ValueNotSerializable { .. } => "E035",
                ErrorKind::FloatOutOfRange { .. } => "E036",
                ErrorKind::ResourceLimitExceeded { .. } => "E043",
                ErrorKind::IncludeIoError { .. } => "E051",
                ErrorKind::IncludeCycle { .. } => "E052",
                ErrorKind::IncludeFileTooLarge { .. } => "E054",
                ErrorKind::IncludeHashMismatch { .. } => "E055",
                ErrorKind::IncludeHashRequired { .. } => "E056",
                ErrorKind::ParseConversion { .. } => "E060",
                ErrorKind::UriParseError { .. } => "E063",
                ErrorKind::CircularDependency { .. } => "E070",
                ErrorKind::MatchExhaustion { .. } => "E071",
                ErrorKind::DuplicateVariable { .. } => "E072",
                ErrorKind::UserError { .. } => "E080",
                ErrorKind::Unimplemented { .. } => "E081",
                ErrorKind::BuilderFinished { .. } => "E082",
                ErrorKind::SchemaViolation { .. } => "E090",
                ErrorKind::KindMismatch { .. } => "E091",
                ErrorKind::CapabilityRequired { .. } => "E044",
                ErrorKind::Internal { .. } => "E099",
            }
        }
        // The actual test just needs to compile.
        let _ = assert_code_exhaustive;
    }

    #[test]
    fn test_error_kind_partial_eq_reflexive() {
        // Smoke test: verify ErrorKind reflexivity for one representative variant.
        // Runtime exhaustiveness is enforced by test_partialeq_all_variants_covered
        // via all_error_kind_variants(). No compile-time guarantee exists for PartialEq
        // (the catch-all _ => false arm prevents that).
        let a = ErrorKind::Internal {
            message: "test".to_string(),
        };
        assert!(a == a);
    }

    #[test]
    fn test_blame_label_positive_polarity() {
        let origin_span = test_span(3, 5, 3, 10);
        let boundary_span = test_span(5, 1, 5, 15);
        let label = BlameLabel {
            origin_span: origin_span.clone(),
            boundary_span: boundary_span.clone(),
            polarity: BlameParity::Positive,
        };
        assert_eq!(label.polarity, BlameParity::Positive);
        assert_eq!(label.origin_span, origin_span);
        assert_eq!(label.boundary_span, boundary_span);
    }

    #[test]
    fn test_blame_label_negative_polarity() {
        let origin_span = test_span(7, 1, 7, 5);
        let boundary_span = test_span(12, 3, 12, 20);
        let label = BlameLabel {
            origin_span,
            boundary_span,
            polarity: BlameParity::Negative,
        };
        assert_eq!(label.polarity, BlameParity::Negative);
    }

    #[test]
    fn test_eval_error_with_blame() {
        let def_span = test_span(7, 1, 7, 5);
        let err = EvalError::type_assert_failed("Int", "String", def_span);

        let origin_span = test_span(3, 5, 3, 10);
        let boundary_span = test_span(7, 1, 7, 15);
        let label = BlameLabel {
            origin_span,
            boundary_span,
            polarity: BlameParity::Positive,
        };

        let err_with_blame = err.with_blame(label.clone());
        assert_eq!(err_with_blame.blame, Some(label));

        // Verify display includes blame information
        let display = format!("{}", err_with_blame);
        assert!(display.contains("blame:"));
        assert!(display.contains("typed side (annotation)"));
    }

    #[test]
    fn test_eval_error_with_blame_negative() {
        let def_span = test_span(10, 1, 10, 5);
        let err = EvalError::type_mismatch("Int", "String", def_span);

        let origin_span = test_span(5, 1, 5, 10);
        let boundary_span = test_span(10, 1, 10, 15);
        let label = BlameLabel {
            origin_span,
            boundary_span,
            polarity: BlameParity::Negative,
        };

        let err_with_blame = err.with_blame(label);

        // Verify display shows negative polarity
        let display = format!("{}", err_with_blame);
        assert!(display.contains("blame:"));
        assert!(display.contains("untyped side (producer)"));
    }

    #[test]
    fn test_blame_co_natural_strategy() {
        // Co-natural strategy: innermost blame label is preserved, outer labels discarded
        let def_span = test_span(10, 1, 10, 5);
        let err = EvalError::type_assert_failed("Int", "String", def_span);

        // First (innermost) blame label
        let inner_label = BlameLabel {
            origin_span: test_span(5, 1, 5, 10),
            boundary_span: test_span(8, 1, 8, 15),
            polarity: BlameParity::Positive,
        };

        // Second (outer) blame label
        let outer_label = BlameLabel {
            origin_span: test_span(1, 1, 1, 5),
            boundary_span: test_span(12, 1, 12, 20),
            polarity: BlameParity::Negative,
        };

        // Apply inner label first
        let err = err.with_blame(inner_label.clone());
        // Apply outer label (should be discarded)
        let err = err.with_blame(outer_label);

        // Should still have the inner label
        assert_eq!(err.blame, Some(inner_label));
    }

    // ── Type diagnostic system tests ────────────────────────────────────────

    #[test]
    fn test_diagnostic_level_bump() {
        assert_eq!(DiagnosticLevel::Info.bump(), DiagnosticLevel::Warn);
        assert_eq!(DiagnosticLevel::Warn.bump(), DiagnosticLevel::Err);
        assert_eq!(DiagnosticLevel::Err.bump(), DiagnosticLevel::Err);
    }

    #[test]
    fn test_type_diagnostic_construction() {
        use crate::test_util::test_span;

        let span = test_span(5, 10, 5, 20);
        let diag = TypeDiagnostic {
            message: "inferred Unknown type".to_string(),
            span: span.clone(),
            code: "T999",
            level: DiagnosticLevel::Warn,
        };

        assert_eq!(diag.message, "inferred Unknown type");
        assert_eq!(diag.span, span);
        assert_eq!(diag.code, "T999");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
    }

    #[test]
    fn test_diagnostic_level_equality() {
        assert_eq!(DiagnosticLevel::Info, DiagnosticLevel::Info);
        assert_eq!(DiagnosticLevel::Warn, DiagnosticLevel::Warn);
        assert_eq!(DiagnosticLevel::Err, DiagnosticLevel::Err);
        assert_ne!(DiagnosticLevel::Info, DiagnosticLevel::Warn);
        assert_ne!(DiagnosticLevel::Warn, DiagnosticLevel::Err);
    }

    // ── Span file-path tests (S-783: spans carry file: Option<Arc<SourceFile>>) ────

    /// Helper: build a Span whose `file` field is populated with a given path.
    /// The span position itself is (line=3, col=5) → (line=3, col=10).
    fn span_with_file(path: &str) -> Span {
        use crate::ast::{Position, SourceFile};
        Span {
            start: Position {
                offset: 10,
                line: 3,
                column: 5,
            },
            end: Position {
                offset: 15,
                line: 3,
                column: 10,
            },
            file: Some(Arc::new(SourceFile {
                path: Arc::from(path),
                content: Arc::from(""),
            })),
        }
    }

    #[test]
    fn test_format_span_location_with_file() {
        // A span with file: Some("prelude.llt") must format as "prelude.llt:3:5-3:10".
        // Regression guard: if format_span_location drops the file prefix, this fails.
        let span = span_with_file("prelude.llt");
        let location = format_span_location(&span);
        assert_eq!(
            location, "prelude.llt:3:5-3:10",
            "format_span_location must prefix with the file path when span.file is Some"
        );
    }

    #[test]
    fn test_format_span_location_without_file() {
        // A span with file: None must format as position-only (no file prefix, no panic).
        // Synthetic spans (Span::origin(), macro-synthesized) always have file: None.
        let span = test_span(3, 5, 3, 10);
        let location = format_span_location(&span);
        assert_eq!(
            location, "3:5-3:10",
            "format_span_location must not show a file prefix when span.file is None"
        );
        // Verify origin span is also panic-free (extra guard for Span::origin()).
        let origin_location = format_span_location(&Span::origin());
        assert!(
            !origin_location.contains('/'),
            "origin span location must not contain a file path: {origin_location}"
        );
    }

    #[test]
    fn test_display_error_with_file_in_definition_span() {
        // When definition_span carries a SourceFile, the error Display line must
        // include the file path in the "defined at ..." clause.
        //
        // Expected: "[E099] oops (defined at prelude.llt:3:5-3:10)"
        let span = span_with_file("prelude.llt");
        let err = EvalError::internal("oops".to_string(), span);
        let display = format!("{err}");

        assert!(
            display.contains("prelude.llt:"),
            "error display must contain the source file path; got: {display}"
        );
        assert!(
            display.contains("defined at prelude.llt:3:5-3:10"),
            "error display must show 'defined at prelude.llt:3:5-3:10'; got: {display}"
        );
    }

    #[test]
    fn test_display_error_synthetic_span_no_panic() {
        // Span::origin() (file: None) must display gracefully — no panic, no file shown.
        // Mutation guard: if format_span_location panics on None, this test catches it.
        let err = EvalError::internal("synthetic error".to_string(), Span::origin());
        // Must not panic.
        let display = format!("{err}");
        assert!(
            display.contains("[E099]"),
            "display must include error code; got: {display}"
        );
        // Origin span has no file path — no slash, no '.llt', no ':' before the position.
        // The position itself is "1:1-1:1" (origin) — verify no file prefix.
        assert!(
            display.contains("defined at 1:1-1:1"),
            "synthetic span must show position only without file prefix; got: {display}"
        );
        assert!(
            !display.contains(".llt"),
            "synthetic span must not show any file path; got: {display}"
        );
    }

    #[test]
    fn test_display_error_materialization_span_with_file() {
        // When the materialization span carries a different file from the definition span,
        // both file paths must appear in the Display output.
        //
        // Scenario: error defined in "prelude.llt", materialized in "user.llt".
        let def_span = span_with_file("prelude.llt");
        let _mat_span = span_with_file("user.llt");
        // Give mat_span a different position so it differs from def_span.
        let mat_span = {
            use crate::ast::{Position, SourceFile};
            Span {
                start: Position {
                    offset: 20,
                    line: 10,
                    column: 1,
                },
                end: Position {
                    offset: 25,
                    line: 10,
                    column: 6,
                },
                file: Some(Arc::new(SourceFile {
                    path: Arc::from("user.llt"),
                    content: Arc::from(""),
                })),
            }
        };

        let err = EvalError::internal("prelude error".to_string(), def_span)
            .with_materialization_span(mat_span);
        let display = format!("{err}");

        assert!(
            display.contains("prelude.llt:"),
            "display must contain the definition file (prelude.llt); got: {display}"
        );
        assert!(
            display.contains("user.llt:"),
            "display must contain the materialization file (user.llt); got: {display}"
        );
        assert!(
            display.contains("defined at prelude.llt:3:5-3:10"),
            "definition location must be 'prelude.llt:3:5-3:10'; got: {display}"
        );
        assert!(
            display.contains("user.llt:10:1-10:6"),
            "materialization location must be 'user.llt:10:1-10:6'; got: {display}"
        );
    }

    #[test]
    fn test_display_error_stack_frame_with_file() {
        // Stack frames use format_span_location for their "in fn at location" line.
        // When a frame's definition_span carries a SourceFile, the frame location
        // must include the file path.
        //
        // Scenario: error in user code, stack frame from "prelude.llt".
        let user_span = test_span(5, 1, 5, 10); // no file: user input without source file
        let prelude_frame_span = span_with_file("prelude.llt");

        let err = EvalError::internal("inner error".to_string(), user_span)
            .with_frame("map".to_string(), prelude_frame_span);
        let display = format!("{err}");

        assert!(
            display.contains("in map at prelude.llt:3:5-3:10"),
            "stack frame must show 'in map at prelude.llt:3:5-3:10'; got: {display}"
        );
    }

    #[test]
    fn test_display_error_stack_frame_without_file() {
        // Stack frames without file-backed spans must show position-only.
        // Mutation guard: if we accidentally add file prefix for None spans, this fails.
        let def_span = test_span(1, 1, 1, 5);
        let frame_span = test_span(7, 3, 7, 15);

        let err = EvalError::internal("error".to_string(), def_span)
            .with_frame("my_fn".to_string(), frame_span);
        let display = format!("{err}");

        assert!(
            display.contains("in my_fn at 7:3-7:15"),
            "frame without file must show position only 'in my_fn at 7:3-7:15'; got: {display}"
        );
        assert!(
            !display.contains(".llt"),
            "frame without file must not show a file path; got: {display}"
        );
    }

    #[test]
    fn test_display_error_prelude_frame_user_definition() {
        // Realistic scenario: user calls map (a prelude function), which internally
        // errors. The primary error is at a definition site without a file path
        // (no source-backed span), and the stack frame is from prelude.llt.
        //
        // This is the canonical "prelude function backtrace" scenario from S-783:
        // the frame for `map` should show prelude.llt, not the user's file.
        let user_def_span = test_span(2, 5, 2, 20); // user code, no file
        let prelude_map_span = span_with_file("stdlib/prelude.llt");

        let err = EvalError::type_mismatch("Int", "String", user_def_span)
            .with_frame("map".to_string(), prelude_map_span);
        let display = format!("{err}");

        // The error itself (E010) is at user code position
        assert!(
            display.contains("[E010]"),
            "must show error code E010; got: {display}"
        );
        assert!(
            display.contains("defined at 2:5-2:20"),
            "definition must show user span without file; got: {display}"
        );

        // The stack frame for `map` must name the prelude file
        assert!(
            display.contains("in map at stdlib/prelude.llt:"),
            "prelude frame must show 'stdlib/prelude.llt:' in location; got: {display}"
        );
    }
}
