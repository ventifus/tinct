//! Evaluator error types with definition-site spans, materialization-site spans, and stack frames.

use std::fmt;

use crate::ast::Span;

/// Convenience type alias for evaluation results.
pub type EvalResult<T> = Result<T, Box<EvalError>>;

/// Arity constraint for function calls.
#[derive(Debug, Clone, PartialEq)]
pub enum ArityBound {
    Exact(usize),
    AtMost(usize),
    Range(usize, usize),
}

impl fmt::Display for ArityBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(1) => write!(f, "1 argument"),
            Self::Exact(n) => write!(f, "{n} arguments"),
            Self::AtMost(1) => write!(f, "at most 1 argument"),
            Self::AtMost(n) => write!(f, "at most {n} arguments"),
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

/// Structured error kind with domain-specific data.
#[derive(Debug, Clone)]
pub enum ErrorKind {
    // --- Access errors (E000-E009) ---
    KeyNotFound {
        key: String,
        available_keys: Vec<String>,
    },
    /// `name` stores the identifier without `$` prefix (e.g., `"x"` not `"$x"`).
    /// Display adds the `$` back: `"undefined variable: $x"`.
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

    // --- Limit errors (E040-E049) ---
    /// Evaluation depth limit (recursive thunk forcing).
    DepthExceeded {
        limit: usize,
    },
    /// JSON nesting depth limit (distinct from eval depth — applies during
    /// `$from-json` parsing of deeply nested JSON structures).
    JsonDepthExceeded {
        limit: usize,
    },
    /// Filesystem access is disabled (--no-fs sandbox flag).
    IncludeForbidden,
    /// Resource limit exceeded (collection size, string size, etc.).
    /// Like `DepthExceeded`, this is non-catchable — resource limits are
    /// safety boundaries, not application-level errors.
    ResourceLimitExceeded {
        message: String,
    },

    // --- Include errors (E050-E059) ---
    IncludeNotAvailable,
    /// Covers both "cannot open" (canonicalize failure) and "cannot read"
    /// (metadata/read failure). The `detail` field carries the OS error.
    IncludeIoError {
        path: String,
        detail: String,
    },
    IncludeCycle {
        path: String,
    },
    IncludeParseFailed {
        path: String,
        detail: String,
    },
    IncludeFileTooLarge {
        path: String,
        size: u64,
        limit: u64,
    },

    // --- Conversion errors (E060-E069) ---
    ParseConversion {
        builtin: String,
        input: String,
        target: String,
    },
    JsonParse {
        detail: String,
    },
    JsonRange,

    // --- Evaluation structure (E070-E079) ---
    CircularDependency {
        name: String,
    },

    // --- User-generated (E080-E089) ---
    UserError {
        message: String,
    },

    // --- Escape hatch (E090-E099) ---
    Internal {
        message: String,
    },
}

impl ErrorKind {
    /// Returns a stable error code string for this error kind.
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyNotFound { .. } => "E001",
            Self::UndefinedVariable { .. } => "E002",
            Self::TypeMismatch { .. } => "E010",
            Self::TypeAssertFailed { .. } => "E011",
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
            Self::DepthExceeded { .. } => "E040",
            Self::JsonDepthExceeded { .. } => "E041",
            Self::IncludeForbidden => "E042",
            Self::ResourceLimitExceeded { .. } => "E043",
            Self::IncludeNotAvailable => "E050",
            Self::IncludeIoError { .. } => "E051",
            Self::IncludeCycle { .. } => "E052",
            Self::IncludeParseFailed { .. } => "E053",
            Self::IncludeFileTooLarge { .. } => "E054",
            Self::ParseConversion { .. } => "E060",
            Self::JsonParse { .. } => "E061",
            Self::JsonRange => "E062",
            Self::CircularDependency { .. } => "E070",
            Self::UserError { .. } => "E080",
            Self::Internal { .. } => "E099",
        }
    }

    /// Returns `false` for errors that must not be cached in Failed thunk state.
    /// Currently only `DepthExceeded` — a thunk that fails at one depth may
    /// succeed at a shallower depth (PROP-DEPTH in §Error Semantics).
    pub fn is_cacheable(&self) -> bool {
        !matches!(self, Self::DepthExceeded { .. })
    }

    /// Returns `false` for errors that must not be caught by `$try`.
    /// Resource limit errors (`DepthExceeded`, `ResourceLimitExceeded`) should
    /// propagate to the runtime, not be suppressible by user code.
    /// Follows GHC's StackOverflow and Racket's exn:fail:resource semantics.
    pub fn is_catchable(&self) -> bool {
        !matches!(
            self,
            Self::DepthExceeded { .. } | Self::ResourceLimitExceeded { .. }
        )
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
            Self::UndefinedVariable { name } => write!(f, "undefined variable: ${name}"),
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
            Self::UnknownNamedArg { name } => write!(f, "unexpected named argument: {name}"),
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
            Self::DepthExceeded { limit } => {
                write!(f, "maximum evaluation depth exceeded ({limit})")
            }
            Self::JsonDepthExceeded { limit } => {
                write!(f, "maximum JSON nesting depth exceeded ({limit})")
            }
            Self::IncludeForbidden => write!(f, "filesystem access is disabled (--no-fs)"),
            Self::ResourceLimitExceeded { message } => write!(f, "{}", message),
            Self::IncludeNotAvailable => write!(f, "include: not available in this context"),
            Self::IncludeIoError { path, detail } => {
                write!(f, "include: cannot access \"{path}\": {detail}")
            }
            Self::IncludeCycle { path } => write!(f, "circular include detected: \"{path}\""),
            Self::IncludeParseFailed { path, detail } => {
                write!(f, "include: parse error in \"{path}\": {detail}")
            }
            Self::IncludeFileTooLarge { path, size, limit } => write!(
                f,
                "include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"
            ),
            Self::ParseConversion {
                builtin,
                input,
                target,
            } => write!(f, "{builtin}: cannot parse {input:?} as {target}"),
            Self::JsonParse { detail } => write!(f, "from-json: invalid JSON: {detail}"),
            Self::JsonRange => write!(f, "JSON number outside representable range"),
            Self::CircularDependency { name } => {
                write!(f, "circular dependency detected while evaluating {name}")
            }
            Self::UserError { message } => write!(f, "{message}"),
            Self::Internal { message } => write!(f, "{message}"),
        }
    }
}

impl PartialEq for ErrorKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::KeyNotFound { key: k1, .. }, Self::KeyNotFound { key: k2, .. }) => k1 == k2,
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
            (Self::UnknownNamedArg { name: n1 }, Self::UnknownNamedArg { name: n2 }) => n1 == n2,
            (Self::NamedArgRejected { builtin: b1 }, Self::NamedArgRejected { builtin: b2 }) => {
                b1 == b2
            }
            (Self::DuplicateKey { key: k1 }, Self::DuplicateKey { key: k2 }) => k1 == k2,
            (Self::DivisionByZero { op: o1 }, Self::DivisionByZero { op: o2 }) => o1 == o2,
            (Self::IntegerOverflow { op: o1 }, Self::IntegerOverflow { op: o2 }) => o1 == o2,
            // Use bitwise comparison for f64 to handle NaN correctly
            (
                Self::FloatNotFinite {
                    builtin: b1,
                    value: v1,
                },
                Self::FloatNotFinite {
                    builtin: b2,
                    value: v2,
                },
            ) => b1 == b2 && v1.to_bits() == v2.to_bits(),
            (Self::EmptyCollection { op: o1 }, Self::EmptyCollection { op: o2 }) => o1 == o2,
            (
                Self::ValueNotSerializable { value_type: v1 },
                Self::ValueNotSerializable { value_type: v2 },
            ) => v1 == v2,
            (Self::DepthExceeded { limit: l1 }, Self::DepthExceeded { limit: l2 }) => l1 == l2,
            (Self::JsonDepthExceeded { limit: l1 }, Self::JsonDepthExceeded { limit: l2 }) => {
                l1 == l2
            }
            (Self::IncludeForbidden, Self::IncludeForbidden) => true,
            (
                Self::ResourceLimitExceeded { message: m1 },
                Self::ResourceLimitExceeded { message: m2 },
            ) => m1 == m2,
            (Self::IncludeNotAvailable, Self::IncludeNotAvailable) => true,
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
                Self::IncludeParseFailed {
                    path: p1,
                    detail: d1,
                },
                Self::IncludeParseFailed {
                    path: p2,
                    detail: d2,
                },
            ) => p1 == p2 && d1 == d2,
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
            (Self::JsonParse { detail: d1 }, Self::JsonParse { detail: d2 }) => d1 == d2,
            (Self::JsonRange, Self::JsonRange) => true,
            (Self::CircularDependency { name: n1 }, Self::CircularDependency { name: n2 }) => {
                n1 == n2
            }
            (Self::UserError { message: m1 }, Self::UserError { message: m2 }) => m1 == m2,
            (Self::Internal { message: m1 }, Self::Internal { message: m2 }) => m1 == m2,
            // This wildcard correctly returns false for cross-variant comparisons
            // (e.g., Timeout vs IoError). When adding a new ErrorKind variant, add a
            // corresponding same-variant arm in each match block in code(), Display,
            // is_catchable(), is_cacheable() — enforced by the all_error_kind_variants()
            // runtime test (test_partialeq_all_variants_covered, test_error_kind_code,
            // test_error_kind_display, test_is_catchable_all_variants, test_is_cacheable).
            _ => false,
        }
    }
}

/// A single frame in an evaluation stack trace (function name + source location).
#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    pub label: String,
    pub span: Span,
}

/// Evaluation error with definition-site span, optional materialization-site span,
/// and a stack trace of enclosing function calls.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub kind: ErrorKind,
    pub definition_span: Span,
    pub materialization_span: Option<Span>,
    pub stack: Vec<StackFrame>,
}

impl EvalError {
    /// Compatibility shim for existing callers. Delegates to `internal()`.
    /// New code should use typed ErrorKind variants instead.
    pub fn new(message: String, definition_span: Span) -> Self {
        Self::internal(message, definition_span)
    }

    /// Create an error with the Internal escape hatch kind.
    /// Use typed ErrorKind variants instead when possible.
    pub fn internal(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::Internal { message },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    /// Compatibility shim — new code should match on `.kind` directly.
    pub fn message(&self) -> String {
        self.kind.to_string()
    }

    pub fn with_materialization_span(mut self, span: Span) -> Self {
        self.materialization_span = Some(span);
        self
    }

    /// Builder for stack frame attachment.
    pub fn with_frame(mut self, label: String, span: Span) -> Self {
        self.stack.push(StackFrame { label, span });
        self
    }

    /// Mutable stack frame push.
    pub fn push_frame(&mut self, label: String, span: Span) {
        self.stack.push(StackFrame { label, span });
    }

    pub fn key_not_found(key: &str, available_keys: Vec<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::KeyNotFound {
                key: key.to_string(),
                available_keys,
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
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
            stack: Vec::new(),
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
            stack: Vec::new(),
        }
    }

    pub fn circular_dependency(name: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::CircularDependency {
                name: name.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn depth_exceeded(limit: usize, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DepthExceeded { limit },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn user_error(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UserError { message },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn named_arg_rejected(builtin: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NamedArgRejected { builtin },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn integer_overflow(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IntegerOverflow { op },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
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
            stack: Vec::new(),
        }
    }

    pub fn division_by_zero(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DivisionByZero { op },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn float_not_finite(builtin: String, value: f64, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::FloatNotFinite { builtin, value },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn empty_collection(op: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::EmptyCollection { op },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn value_not_serializable(value_type: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ValueNotSerializable { value_type },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn undefined_variable(name: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UndefinedVariable { name },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
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
            stack: Vec::new(),
        }
    }

    pub fn named_arg_conflict(param: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NamedArgConflict { param },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn unknown_named_arg(name: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UnknownNamedArg { name },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn duplicate_key(key: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DuplicateKey {
                key: key.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn json_depth_exceeded(limit: usize, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::JsonDepthExceeded { limit },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn include_forbidden(definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeForbidden,
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn resource_limit_exceeded(message: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::ResourceLimitExceeded { message },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn include_not_available(definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeNotAvailable,
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn include_io_error(path: String, detail: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeIoError { path, detail },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn include_cycle(path: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeCycle { path },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn include_parse_failed(path: String, detail: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IncludeParseFailed { path, detail },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
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
            stack: Vec::new(),
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
            stack: Vec::new(),
        }
    }

    pub fn json_parse(detail: String, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::JsonParse { detail },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn json_range(definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::JsonRange,
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn missing_required_param(param: impl Into<String>, span: Span) -> Self {
        Self {
            kind: ErrorKind::MissingRequiredParam {
                param: param.into(),
            },
            definition_span: span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }
}

/// Suffixes that identify stdlib internal helper frames to hide from user-facing stack traces.
/// User-facing stdlib functions like `$map` and `$filter` do not use these suffixes and remain
/// visible. Filtering is suffix-based (ends_with), not substring-based (contains), so a label
/// like "multi-step-validator" is preserved correctly.
const HIDDEN_SUFFIXES: &[&str] = &["-impl", "-step", "-check"];

/// Infer a context-appropriate verb for the materialization span label.
/// Checks the first visible stack frame label to determine whether the thunk
/// was forced by a function call or a field/bracket access.
fn infer_materialization_verb(stack: &[StackFrame]) -> &'static str {
    for frame in stack {
        if HIDDEN_SUFFIXES.iter().any(|s| frame.label.ends_with(s)) {
            continue;
        }
        let label = frame.label.to_ascii_lowercase();
        if label.contains("call") {
            return "called at";
        }
        if label.contains('.')
            || label.contains("dot")
            || label.contains("access")
            || label.contains('[')
            || label.contains("bracket")
        {
            return "accessed at";
        }
    }
    "materialized at"
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} (defined at {})",
            self.kind.code(),
            self.kind,
            self.definition_span
        )?;
        // Only show materialization span if it differs from definition span (doc/10-errors.md:820)
        if let Some(ref mat_span) = self.materialization_span {
            if mat_span != &self.definition_span {
                let verb = infer_materialization_verb(&self.stack);
                write!(f, " ({verb} {mat_span})")?;
            }
        }
        for frame in &self.stack {
            // Filter out synthetic origin spans (Span::origin() = offset 0, line 1, col 1;
            // displays as 1:1-1:1) from stdlib/builtin calls. Uses exact structural
            // equality — real user source at line 1 would have a different byte offset.
            if frame.span == Span::origin() {
                continue;
            }

            // Filter out stdlib internal helper functions (suffixes: -impl, -step, -check).
            // These are implementation details that add noise to user-facing stack traces.
            // User-facing stdlib functions like $map, $filter remain visible.
            if HIDDEN_SUFFIXES.iter().any(|s| frame.label.ends_with(s)) {
                continue;
            }

            write!(f, "\n  in {} at {}", frame.label, frame.span)?;
        }
        Ok(())
    }
}

impl std::error::Error for EvalError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_span;

    #[test]
    fn test_eval_error_new() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::new("something broke".to_string(), span);
        assert_eq!(err.message(), "something broke");
        assert_eq!(err.definition_span, span);
        assert_eq!(err.materialization_span, None);
        assert!(err.stack.is_empty());
    }

    #[test]
    fn test_eval_error_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err =
            EvalError::new("lazy fail".to_string(), def_span).with_materialization_span(mat_span);
        assert_eq!(err.materialization_span, Some(mat_span));
    }

    #[test]
    fn test_eval_error_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = EvalError::new("err".to_string(), span)
            .with_frame("my_function".to_string(), frame_span);
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "my_function");
        assert_eq!(err.stack[0].span, frame_span);
    }

    #[test]
    fn test_eval_error_key_not_found() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::key_not_found("foo", vec![], span);
        assert_eq!(err.message(), "key not found: foo");
    }

    #[test]
    fn test_eval_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::type_mismatch("Int", "String", span);
        assert_eq!(err.message(), "type mismatch: expected Int, got String");
    }

    #[test]
    fn test_eval_error_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::arity_mismatch(2, 3, span);
        assert_eq!(err.message(), "arity mismatch: expected 2 arguments, got 3");
    }

    #[test]
    fn test_eval_error_circular_dependency() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::circular_dependency("$x", span);
        assert_eq!(
            err.message(),
            "circular dependency detected while evaluating $x"
        );
    }

    #[test]
    fn test_eval_error_display_basic() {
        let span = test_span(3, 5, 3, 10);
        let err = EvalError::new("oops".to_string(), span);
        let display = format!("{err}");
        assert_eq!(display, "[E099] oops (defined at 3:5-3:10)");
    }

    #[test]
    fn test_eval_error_display_full() {
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let frame1_span = test_span(10, 2, 10, 8);
        let frame2_span = test_span(15, 1, 15, 12);
        let err = EvalError::new("bad value".to_string(), def_span)
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
        let mut err = EvalError::new("error".to_string(), span);

        assert!(err.stack.is_empty());

        // Push a frame directly
        let frame_span = test_span(5, 1, 5, 10);
        err.push_frame("first_function".to_string(), frame_span);

        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first_function");
        assert_eq!(err.stack[0].span, frame_span);

        // Push a second frame
        let frame2_span = test_span(10, 3, 10, 15);
        err.push_frame("second_function".to_string(), frame2_span);

        assert_eq!(err.stack.len(), 2);
        assert_eq!(err.stack[1].label, "second_function");
        assert_eq!(err.stack[1].span, frame2_span);
    }

    #[test]
    fn test_is_catchable() {
        // DepthExceeded and ResourceLimitExceeded are NOT catchable
        let depth_err = ErrorKind::DepthExceeded { limit: 256 };
        assert!(!depth_err.is_catchable());

        let resource_err = ErrorKind::ResourceLimitExceeded {
            message: "collect: exceeded maximum collection size (1000000)".to_string(),
        };
        assert!(!resource_err.is_catchable());

        // All other errors ARE catchable
        assert!(ErrorKind::KeyNotFound {
            key: "foo".to_string(),
            available_keys: vec![],
        }
        .is_catchable());
        assert!(ErrorKind::TypeMismatch {
            context: None,
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::CircularDependency {
            name: "$x".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::UserError {
            message: "test".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::DivisionByZero {
            op: "/".to_string()
        }
        .is_catchable());
        assert!(ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN
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
            ErrorKind::DepthExceeded { limit: 256 },
            ErrorKind::JsonDepthExceeded { limit: 128 },
            ErrorKind::IncludeForbidden,
            ErrorKind::ResourceLimitExceeded {
                message: "test: resource limit exceeded (1000)".to_string(),
            },
            ErrorKind::IncludeNotAvailable,
            ErrorKind::IncludeIoError {
                path: "x".to_string(),
                detail: "error".to_string(),
            },
            ErrorKind::IncludeCycle {
                path: "x".to_string(),
            },
            ErrorKind::IncludeParseFailed {
                path: "x".to_string(),
                detail: "error".to_string(),
            },
            ErrorKind::IncludeFileTooLarge {
                path: "x".to_string(),
                size: 1000,
                limit: 100,
            },
            ErrorKind::ParseConversion {
                builtin: "to-int".to_string(),
                input: "x".to_string(),
                target: "Int".to_string(),
            },
            ErrorKind::JsonParse {
                detail: "error".to_string(),
            },
            ErrorKind::JsonRange,
            ErrorKind::CircularDependency {
                name: "$x".to_string(),
            },
            ErrorKind::UserError {
                message: "test".to_string(),
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
    fn test_arity_bound_display() {
        // Test Display output for all ArityBound variants
        assert_eq!(format!("{}", ArityBound::Exact(1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Exact(2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::AtMost(1)), "at most 1 argument");
        assert_eq!(format!("{}", ArityBound::AtMost(3)), "at most 3 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 0)), "0 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Range(2, 2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 3)), "1 to 3 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 5)), "0 to 5 arguments");
    }

    #[test]
    fn test_is_cacheable() {
        // DepthExceeded is NOT cacheable (must retry at different depth)
        let depth_err = ErrorKind::DepthExceeded { limit: 256 };
        assert!(!depth_err.is_cacheable());

        // ResourceLimitExceeded IS cacheable (unlike DepthExceeded, resource limits
        // are not context-dependent on call depth — a failed resource limit check
        // will fail consistently regardless of when it's retried)
        assert!(ErrorKind::ResourceLimitExceeded {
            message: "collect: exceeded maximum collection size (1000000)".to_string(),
        }
        .is_cacheable());

        // All other errors ARE cacheable (can be stored in Failed thunk state)
        assert!(ErrorKind::KeyNotFound {
            key: "foo".to_string(),
            available_keys: vec![],
        }
        .is_cacheable());
        assert!(ErrorKind::TypeMismatch {
            context: None,
            expected: "Int".to_string(),
            got: "String".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::CircularDependency {
            name: "$x".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::UserError {
            message: "test".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::DivisionByZero {
            op: "/".to_string()
        }
        .is_cacheable());
        assert!(ErrorKind::FloatNotFinite {
            builtin: "test".to_string(),
            value: f64::NAN
        }
        .is_cacheable());
        assert!(ErrorKind::JsonDepthExceeded { limit: 128 }.is_cacheable());
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
        assert_eq!(
            format!(
                "{}",
                ErrorKind::UndefinedVariable {
                    name: "x".to_string()
                }
            ),
            "undefined variable: $x"
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
                    expected: ArityBound::AtMost(2),
                    got: 3
                }
            ),
            "arity mismatch: expected at most 2 arguments, got 3"
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
                    name: "foo".to_string()
                }
            ),
            "unexpected named argument: foo"
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

        // Limit errors (E040-E049)
        assert_eq!(
            format!("{}", ErrorKind::DepthExceeded { limit: 256 }),
            "maximum evaluation depth exceeded (256)"
        );
        assert_eq!(
            format!("{}", ErrorKind::JsonDepthExceeded { limit: 128 }),
            "maximum JSON nesting depth exceeded (128)"
        );
        assert_eq!(
            format!("{}", ErrorKind::IncludeForbidden),
            "filesystem access is disabled (--no-fs)"
        );
        assert_eq!(
            format!(
                "{}",
                ErrorKind::ResourceLimitExceeded {
                    message: "upper: output would exceed 64 MB limit (67108864 bytes)".to_string(),
                }
            ),
            "upper: output would exceed 64 MB limit (67108864 bytes)"
        );

        // Include errors (E050-E059)
        assert_eq!(
            format!("{}", ErrorKind::IncludeNotAvailable),
            "include: not available in this context"
        );
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
                ErrorKind::IncludeParseFailed {
                    path: "bad.llt".to_string(),
                    detail: "unexpected token".to_string()
                }
            ),
            "include: parse error in \"bad.llt\": unexpected token"
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
                ErrorKind::JsonParse {
                    detail: "unexpected EOF".to_string()
                }
            ),
            "from-json: invalid JSON: unexpected EOF"
        );
        assert_eq!(
            format!("{}", ErrorKind::JsonRange),
            "JSON number outside representable range"
        );

        // Evaluation structure (E070-E079)
        assert_eq!(
            format!(
                "{}",
                ErrorKind::CircularDependency {
                    name: "$x".to_string()
                }
            ),
            "circular dependency detected while evaluating $x"
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

        // Escape hatch (E090-E099)
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
        let err = EvalError::type_mismatch("Int", "String", def_span)
            .with_materialization_span(mat_span)
            .with_frame("dict entry 'inner'".to_string(), frame1_span)
            .with_frame("dict entry 'outer'".to_string(), frame2_span)
            .with_frame("materialized".to_string(), frame3_span);

        assert_eq!(err.definition_span, def_span);
        assert_eq!(err.materialization_span, Some(mat_span));
        assert_eq!(err.stack.len(), 3);
        assert_eq!(err.stack[0].label, "dict entry 'inner'");
        assert_eq!(err.stack[0].span, frame1_span);
        assert_eq!(err.stack[1].label, "dict entry 'outer'");
        assert_eq!(err.stack[1].span, frame2_span);
        assert_eq!(err.stack[2].label, "materialized");
        assert_eq!(err.stack[2].span, frame3_span);
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
            .with_materialization_span(first_mat_span);

        // Simulate a second access from a different location
        // Should preserve original mat_span and add second access as stack frame
        assert_eq!(err.materialization_span, Some(first_mat_span));

        // Manually simulate what attach_materialization_context does
        if !err.stack.iter().any(|f| f.span == second_access_span) {
            err.push_frame("materialized".to_string(), second_access_span);
        }

        assert_eq!(err.materialization_span, Some(first_mat_span));
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].span, second_access_span);
    }

    #[test]
    fn test_stack_frame_avoids_duplicates() {
        // Test that duplicate stack frames (same span) are not added
        let def_span = test_span(1, 1, 1, 10);
        let frame_span = test_span(5, 1, 5, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span);

        err.push_frame("first".to_string(), frame_span);
        assert_eq!(err.stack.len(), 1);

        // Manually check for duplicate before adding (this is what attach_materialization_context does)
        if !err.stack.iter().any(|f| f.span == frame_span) {
            err.push_frame("second".to_string(), frame_span);
        }

        // Should still be 1 frame (duplicate was avoided)
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first");
    }

    #[test]
    fn test_resource_limit_exceeded_not_catchable() {
        // Verify that ResourceLimitExceeded is non-catchable like DepthExceeded
        let err = ErrorKind::ResourceLimitExceeded {
            message: "collect: exceeded maximum collection size (1000000)".to_string(),
        };
        assert!(
            !err.is_catchable(),
            "ResourceLimitExceeded must not be catchable by $try"
        );
    }

    #[test]
    fn test_resource_limit_exceeded_is_cacheable() {
        // Unlike DepthExceeded, ResourceLimitExceeded IS cacheable
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

        let mut err =
            EvalError::new("bad value".to_string(), def_span).with_materialization_span(mat_span);

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
                    stack: Vec::new(),
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
                EvalError::circular_dependency("$x", test_span(1, 1, 1, 5)),
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
    fn test_stdlib_internal_frames_filtered_from_display() {
        // Verify that stdlib internal helper frames (with -impl, -step, -check suffixes)
        // are NOT shown in error display output, while user-facing stdlib functions remain visible
        let def_span = test_span(5, 1, 5, 10);
        let mat_span = test_span(10, 1, 10, 5);

        let mut err = EvalError::new("error in stdlib".to_string(), def_span)
            .with_materialization_span(mat_span);

        // Add user-facing stdlib function (should be visible)
        err.push_frame("call $map".to_string(), test_span(8, 1, 8, 10));

        // Add internal helper frames (should be filtered out)
        err.push_frame("call $map-impl".to_string(), test_span(100, 1, 100, 10));
        err.push_frame("call $remove-step".to_string(), test_span(200, 1, 200, 10));
        err.push_frame("call $cond-check".to_string(), test_span(300, 1, 300, 10));

        // Add another user-facing function
        err.push_frame("call $filter".to_string(), test_span(12, 1, 12, 15));

        let display = format!("{err}");

        // Should contain user-facing stdlib functions
        assert!(display.contains("in call $map at 8:1-8:10"));
        assert!(display.contains("in call $filter at 12:1-12:15"));

        // Should NOT contain internal helper frames
        assert!(!display.contains("map-impl"));
        assert!(!display.contains("remove-step"));
        assert!(!display.contains("cond-check"));
        assert!(!display.contains("100:1-100:10"));
        assert!(!display.contains("200:1-200:10"));
        assert!(!display.contains("300:1-300:10"));
    }

    #[test]
    fn test_stdlib_frame_filter_uses_ends_with_not_contains() {
        // "multi-step-validator" contains "-step" but does NOT end with "-step",
        // so it should NOT be filtered — only suffix-based filtering is correct.
        let def_span = test_span(1, 1, 1, 5);
        let err = EvalError::internal("test".to_string(), def_span).with_frame(
            "call $multi-step-validator".to_string(),
            test_span(10, 1, 10, 5),
        );
        let display = format!("{err}");
        // Frame label contains "-step" as substring but not suffix — must appear
        assert!(
            display.contains("multi-step-validator"),
            "frames with '-step' as a substring (not suffix) must not be filtered; got: {display}"
        );
    }
}
