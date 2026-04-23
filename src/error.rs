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
            Self::Exact(n) => write!(f, "{n} arguments"),
            Self::AtMost(n) => write!(f, "at most {n} arguments"),
            Self::Range(lo, hi) => write!(f, "{lo} to {hi} arguments"),
        }
    }
}

/// Structured error kind with domain-specific data.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    // --- Access errors (E000-E009) ---
    KeyNotFound {
        key: String,
    },
    /// `name` stores the identifier without `$` prefix (e.g., `"x"` not `"$x"`).
    /// Display adds the `$` back: `"undefined variable: $x"`.
    UndefinedVariable {
        name: String,
    },

    // --- Type errors (E010-E019) ---
    /// Runtime type mismatch from evaluator or builtin dispatch.
    /// `context` carries the builtin name (e.g., `"merge"`) when the mismatch
    /// originates from a builtin; `None` for generic evaluator mismatches.
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
            Self::NamedArgConflict { .. } => "E021",
            Self::UnknownNamedArg { .. } => "E022",
            Self::NamedArgRejected { .. } => "E023",
            Self::DuplicateKey { .. } => "E030",
            Self::DivisionByZero { .. } => "E031",
            Self::IntegerOverflow { .. } => "E032",
            Self::FloatNotFinite { .. } => "E033",
            Self::EmptyCollection { .. } => "E034",
            Self::DepthExceeded { .. } => "E040",
            Self::JsonDepthExceeded { .. } => "E041",
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
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyNotFound { key } => write!(f, "key not found: {key}"),
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
            Self::DepthExceeded { limit } => {
                write!(f, "maximum evaluation depth exceeded ({limit})")
            }
            Self::JsonDepthExceeded { limit } => {
                write!(f, "maximum JSON nesting depth exceeded ({limit})")
            }
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
    pub fn new(message: impl Into<String>, definition_span: Span) -> Self {
        Self::internal(message, definition_span)
    }

    /// Create an error with the Internal escape hatch kind.
    /// Use typed ErrorKind variants instead when possible.
    pub fn internal(message: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::Internal {
                message: message.into(),
            },
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
    pub fn with_frame(mut self, label: impl Into<String>, span: Span) -> Self {
        self.stack.push(StackFrame {
            label: label.into(),
            span,
        });
        self
    }

    /// Mutable stack frame push.
    pub fn push_frame(&mut self, label: impl Into<String>, span: Span) {
        self.stack.push(StackFrame {
            label: label.into(),
            span,
        });
    }

    pub fn key_not_found(key: &str, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::KeyNotFound {
                key: key.to_string(),
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

    pub fn user_error(message: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::UserError {
                message: message.into(),
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn named_arg_rejected(builtin: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::NamedArgRejected {
                builtin: builtin.into(),
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn integer_overflow(op: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::IntegerOverflow { op: op.into() },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn type_mismatch_ctx(
        context: impl Into<String>,
        expected: &str,
        got: &str,
        definition_span: Span,
    ) -> Self {
        Self {
            kind: ErrorKind::TypeMismatch {
                context: Some(context.into()),
                expected: expected.to_string(),
                got: got.to_string(),
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn division_by_zero(op: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::DivisionByZero { op: op.into() },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn float_not_finite(builtin: impl Into<String>, value: f64, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::FloatNotFinite {
                builtin: builtin.into(),
                value,
            },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn empty_collection(op: impl Into<String>, definition_span: Span) -> Self {
        Self {
            kind: ErrorKind::EmptyCollection { op: op.into() },
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }
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
        if let Some(ref mat_span) = self.materialization_span {
            write!(f, " (materialized at {mat_span})")?;
        }
        for frame in &self.stack {
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
        let err = EvalError::new("something broke", span);
        assert_eq!(err.message(), "something broke");
        assert_eq!(err.definition_span, span);
        assert_eq!(err.materialization_span, None);
        assert!(err.stack.is_empty());
    }

    #[test]
    fn test_eval_error_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err = EvalError::new("lazy fail", def_span).with_materialization_span(mat_span);
        assert_eq!(err.materialization_span, Some(mat_span));
    }

    #[test]
    fn test_eval_error_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = EvalError::new("err", span).with_frame("my_function", frame_span);
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "my_function");
        assert_eq!(err.stack[0].span, frame_span);
    }

    #[test]
    fn test_eval_error_key_not_found() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::key_not_found("foo", span);
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
        let err = EvalError::new("oops", span);
        let display = format!("{err}");
        assert_eq!(display, "[E099] oops (defined at 3:5-3:10)");
    }

    #[test]
    fn test_eval_error_display_full() {
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let frame1_span = test_span(10, 2, 10, 8);
        let frame2_span = test_span(15, 1, 15, 12);
        let err = EvalError::new("bad value", def_span)
            .with_materialization_span(mat_span)
            .with_frame("outer", frame1_span)
            .with_frame("inner", frame2_span);
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
        let mut err = EvalError::new("error", span);

        assert!(err.stack.is_empty());

        // Push a frame directly
        let frame_span = test_span(5, 1, 5, 10);
        err.push_frame("first_function", frame_span);

        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first_function");
        assert_eq!(err.stack[0].span, frame_span);

        // Push a second frame
        let frame2_span = test_span(10, 3, 10, 15);
        err.push_frame("second_function", frame2_span);

        assert_eq!(err.stack.len(), 2);
        assert_eq!(err.stack[1].label, "second_function");
        assert_eq!(err.stack[1].span, frame2_span);
    }
}
