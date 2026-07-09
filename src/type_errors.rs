//! Typed TypeError infrastructure — per-error structs and a discriminated enum.
//!
//! Each type error category has its own struct with domain-specific fields,
//! enabling pattern matching, programmatic introspection, and IDE integration
//! without string-parsing. The `GenericTypeError` variant serves as a migration
//! escape hatch for errors not yet migrated to typed variants.
//!
//! ## Call-chain context (B-374, B-379)
//!
//! Every `TypeErrorTyped` carries a `call_stack: Vec<TypeSpanFrame>` that records
//! the chain of call sites that led to the error.  Each frame is a label
//! (e.g. `"in call to \`map\`"`) paired with the call-site [`Span`].
//!
//! Frames are pushed **outward** as errors bubble up through `check_call` and
//! `check_call_with_scheme`.  The innermost call site is at index 0; the
//! outermost is last.  `format_type_error` renders the stack below the primary
//! error snippet as:
//!
//! ```text
//!   = note: in call to `map`
//!    --> src/main.llt:10:5
//! ```
//!
//! This also fixes B-379: when an error originates inside a macro expansion or
//! prelude function (whose span byte-offsets belong to prelude.llt), the
//! call-site frame correctly identifies the user's source location.

use std::fmt;

use crate::ast::Span;
use crate::type_def::{Kind, Type};

// ────────────────────────────────────────────────────────────────────────────────
// TypeSpanFrame — a single call-chain context frame
// ────────────────────────────────────────────────────────────────────────────────

/// A single frame in the type-error call-chain context stack.
///
/// `label` is a human-readable description of the call site,
/// e.g. `"in call to \`map\`"` or `"in call to anonymous function"`.
/// `span` is the source location of the call expression (the entire `[f arg ...]`).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeSpanFrame {
    pub label: String,
    pub span: Span,
}

impl TypeSpanFrame {
    /// Construct a frame for a named function call.
    pub fn call(name: &str, span: Span) -> Self {
        TypeSpanFrame {
            label: format!("call to `{name}`"),
            span,
        }
    }

    /// Construct a frame for an anonymous (non-VarRef) call.
    pub fn call_anon(span: Span) -> Self {
        TypeSpanFrame {
            label: "call to anonymous function".to_string(),
            span,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────────
// Per-error structs
// ────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct ArityMismatch {
    pub expected: usize,
    pub got: usize,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
    /// Name of the function being called, if known (e.g. "if", "[f ...]").
    pub callee: Option<String>,
    /// Parameter names (or "name: Type" descriptions) to include in the error message.
    pub params: Vec<String>,
    /// Types of the actual arguments supplied (best-effort, may be empty).
    pub got_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UndefinedVariable {
    pub name: String,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UndefinedType {
    pub name: String,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnificationFailure {
    pub expected: Type,
    pub got: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldNotFound {
    pub field: String,
    pub record_type: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotARecord {
    pub actual: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotAFunction {
    pub actual: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
    /// The expression being called, if known (e.g. "task", "[task ...]").
    pub callee: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeAssertFailed {
    pub asserted: Type,
    pub actual: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NonExhaustiveMatch {
    /// Missing constructor/pattern names.
    pub missing: Vec<String>,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlappingInstancePatterns {
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsistencyViolation {
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoverageViolation {
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceContainsUnknown {
    pub message: String,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KindMismatch {
    pub expected: Kind,
    pub actual: Type,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

/// Catch-all for errors not yet migrated to typed variants.
/// New construction sites MUST NOT use this — use a typed variant.
/// Existing GenericTypeError instances should be migrated to typed variants over time.
#[derive(Debug, Clone, PartialEq)]
pub struct GenericTypeError {
    pub message: String,
    pub span: Span,
    pub notes: Vec<String>,
    pub call_stack: Vec<TypeSpanFrame>,
}

// ────────────────────────────────────────────────────────────────────────────────
// TypeError enum
// ────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum TypeErrorTyped {
    ArityMismatch(ArityMismatch),
    UndefinedVariable(UndefinedVariable),
    UndefinedType(UndefinedType),
    UnificationFailure(UnificationFailure),
    FieldNotFound(FieldNotFound),
    NotARecord(NotARecord),
    NotAFunction(NotAFunction),
    TypeAssertFailed(TypeAssertFailed),
    NonExhaustiveMatch(NonExhaustiveMatch),
    OverlappingInstancePatterns(OverlappingInstancePatterns),
    ConsistencyViolation(ConsistencyViolation),
    CoverageViolation(CoverageViolation),
    InstanceContainsUnknown(InstanceContainsUnknown),
    KindMismatch(KindMismatch),
    Generic(GenericTypeError),
}

impl TypeErrorTyped {
    // ── Bridge constructors ─────────────────────────────────────────────────
    // These mirror the legacy TypeError API so existing call sites compile.
    // Each produces a `Generic` variant; call sites should migrate to typed variants.

    pub fn new(message: impl Into<String>, span: Span) -> Self {
        TypeErrorTyped::Generic(GenericTypeError {
            message: message.into(),
            span,
            notes: Vec::new(),
            call_stack: Vec::new(),
        })
    }

    /// Builder method: attach an explicit error code and return `self`.
    ///
    /// Legacy bridge — codes are intrinsic to the variant after T-1107.
    /// This is a no-op on typed variants (code is determined by the variant itself).
    pub fn with_code(self, _code: impl Into<String>) -> Self {
        // After T-1107, error codes are intrinsic to the variant.
        // During the transition, just ignore the code.
        self
    }

    pub fn type_mismatch(expected: &Type, got: &Type, span: Span) -> Self {
        Self::new(format!("cannot unify {expected} with {got}"), span)
    }

    pub fn field_not_found(field: &str, record_type: &Type, span: Span) -> Self {
        Self::new(format!("field '{field}' not found in {record_type}"), span)
    }

    pub fn not_a_record(ty: &Type, span: Span) -> Self {
        Self::new(format!("expected Dict, got {ty}"), span)
    }

    pub fn not_a_function(ty: &Type, span: Span) -> Self {
        let mut notes = Vec::new();
        if let Type::Error(payload) = ty {
            for root_cause in payload.iter() {
                let rc_span = root_cause.span();
                let sf = &rc_span.file;
                let location = format!(
                    "{}:{}:{}",
                    sf.path, rc_span.start.line, rc_span.start.column
                );
                notes.push(format!(
                    "  = note: caused by error at {location}: {}",
                    root_cause.message()
                ));
            }
        }
        TypeErrorTyped::NotAFunction(NotAFunction {
            actual: ty.clone(),
            span,
            notes,
            call_stack: Vec::new(),
            callee: None,
        })
    }

    pub fn undefined_variable(name: &str, span: Span) -> Self {
        Self::new(format!("undefined variable: {name}"), span)
    }

    pub fn undefined_type(name: &str, span: Span) -> Self {
        Self::new(format!("undefined type: {name}"), span)
    }

    pub fn kind_mismatch(expected_kind: &str, got: &str, span: Span) -> Self {
        Self::new(
            format!("kind mismatch: expected `{expected_kind}`, got {got}"),
            span,
        )
    }

    /// Add a note to this error.
    pub fn add_note(&mut self, note: impl Into<String>) {
        let note = note.into();
        match self {
            Self::ArityMismatch(e) => e.notes.push(note),
            Self::UndefinedVariable(e) => e.notes.push(note),
            Self::UndefinedType(e) => e.notes.push(note),
            Self::UnificationFailure(e) => e.notes.push(note),
            Self::FieldNotFound(e) => e.notes.push(note),
            Self::NotARecord(e) => e.notes.push(note),
            Self::NotAFunction(e) => e.notes.push(note),
            Self::TypeAssertFailed(e) => e.notes.push(note),
            Self::NonExhaustiveMatch(e) => e.notes.push(note),
            Self::OverlappingInstancePatterns(e) => e.notes.push(note),
            Self::ConsistencyViolation(e) => e.notes.push(note),
            Self::CoverageViolation(e) => e.notes.push(note),
            Self::InstanceContainsUnknown(e) => e.notes.push(note),
            Self::KindMismatch(e) => e.notes.push(note),
            Self::Generic(e) => e.notes.push(note),
        }
    }

    // ── Call-chain context (B-374, B-379) ──────────────────────────────────

    /// Push a call-chain frame onto this error's stack (mutating).
    ///
    /// Frames are appended outward as errors bubble up through `check_call` /
    /// `check_call_with_scheme`.  The innermost call site is pushed first (index 0);
    /// the outermost call site is pushed last.
    pub fn push_frame(&mut self, frame: TypeSpanFrame) {
        match self {
            Self::ArityMismatch(e) => e.call_stack.push(frame),
            Self::UndefinedVariable(e) => e.call_stack.push(frame),
            Self::UndefinedType(e) => e.call_stack.push(frame),
            Self::UnificationFailure(e) => e.call_stack.push(frame),
            Self::FieldNotFound(e) => e.call_stack.push(frame),
            Self::NotARecord(e) => e.call_stack.push(frame),
            Self::NotAFunction(e) => e.call_stack.push(frame),
            Self::TypeAssertFailed(e) => e.call_stack.push(frame),
            Self::NonExhaustiveMatch(e) => e.call_stack.push(frame),
            Self::OverlappingInstancePatterns(e) => e.call_stack.push(frame),
            Self::ConsistencyViolation(e) => e.call_stack.push(frame),
            Self::CoverageViolation(e) => e.call_stack.push(frame),
            Self::InstanceContainsUnknown(e) => e.call_stack.push(frame),
            Self::KindMismatch(e) => e.call_stack.push(frame),
            Self::Generic(e) => e.call_stack.push(frame),
        }
    }

    /// Builder variant: push a call-chain frame and return `self`.
    pub fn with_frame(mut self, frame: TypeSpanFrame) -> Self {
        self.push_frame(frame);
        self
    }

    /// Returns the call-chain context frames for this error.
    ///
    /// Frames are ordered innermost-first (index 0 = the directly enclosing call).
    /// `format_type_error` renders these below the primary error snippet.
    pub fn call_stack(&self) -> &[TypeSpanFrame] {
        match self {
            Self::ArityMismatch(e) => &e.call_stack,
            Self::UndefinedVariable(e) => &e.call_stack,
            Self::UndefinedType(e) => &e.call_stack,
            Self::UnificationFailure(e) => &e.call_stack,
            Self::FieldNotFound(e) => &e.call_stack,
            Self::NotARecord(e) => &e.call_stack,
            Self::NotAFunction(e) => &e.call_stack,
            Self::TypeAssertFailed(e) => &e.call_stack,
            Self::NonExhaustiveMatch(e) => &e.call_stack,
            Self::OverlappingInstancePatterns(e) => &e.call_stack,
            Self::ConsistencyViolation(e) => &e.call_stack,
            Self::CoverageViolation(e) => &e.call_stack,
            Self::InstanceContainsUnknown(e) => &e.call_stack,
            Self::KindMismatch(e) => &e.call_stack,
            Self::Generic(e) => &e.call_stack,
        }
    }

    /// Returns the stable type error code for this error (legacy bridge).
    ///
    /// Maps typed variants to their legacy T-codes for `format_type_error` compatibility.
    pub fn code(&self) -> &str {
        match self {
            Self::ArityMismatch(_) => "T001",
            Self::UndefinedVariable(_) | Self::UndefinedType(_) => "T002",
            Self::UnificationFailure(_)
            | Self::FieldNotFound(_)
            | Self::NotARecord(_)
            | Self::NotAFunction(_) => "T003",
            Self::TypeAssertFailed(_) | Self::NonExhaustiveMatch(_) => "T004",
            Self::OverlappingInstancePatterns(_) => "T014",
            Self::ConsistencyViolation(_) => "T015",
            Self::CoverageViolation(_) => "T016",
            Self::InstanceContainsUnknown(_) => "T017",
            Self::KindMismatch(_) => "T091",
            Self::Generic(e) => {
                // Replicate the legacy message-pattern dispatch for Generic variants.
                let msg = &e.message;
                if msg.starts_with("arity mismatch") {
                    "T001"
                } else if msg.starts_with("undefined variable") || msg.starts_with("undefined type")
                {
                    "T002"
                } else if msg.starts_with("cannot unify")
                    || msg.starts_with("field '")
                    || msg.starts_with("expected record type")
                    || msg.starts_with("expected function type")
                    || msg.starts_with("type mismatch")
                {
                    "T003"
                } else if msg.contains("type assert") || msg.starts_with("non-exhaustive match") {
                    "T004"
                } else if msg.starts_with("overlapping instance patterns") {
                    "T014"
                } else if msg.starts_with("consistency violation") {
                    "T015"
                } else if msg.starts_with("coverage violation") {
                    "T016"
                } else if msg.starts_with("kind mismatch") {
                    "T091"
                } else {
                    "T000"
                }
            }
        }
    }

    // ── Accessors ───────────────────────────────────────────────────────────

    pub fn span(&self) -> &Span {
        match self {
            Self::ArityMismatch(e) => &e.span,
            Self::UndefinedVariable(e) => &e.span,
            Self::UndefinedType(e) => &e.span,
            Self::UnificationFailure(e) => &e.span,
            Self::FieldNotFound(e) => &e.span,
            Self::NotARecord(e) => &e.span,
            Self::NotAFunction(e) => &e.span,
            Self::TypeAssertFailed(e) => &e.span,
            Self::NonExhaustiveMatch(e) => &e.span,
            Self::OverlappingInstancePatterns(e) => &e.span,
            Self::ConsistencyViolation(e) => &e.span,
            Self::CoverageViolation(e) => &e.span,
            Self::InstanceContainsUnknown(e) => &e.span,
            Self::KindMismatch(e) => &e.span,
            Self::Generic(e) => &e.span,
        }
    }

    pub fn notes(&self) -> &[String] {
        match self {
            Self::ArityMismatch(e) => &e.notes,
            Self::UndefinedVariable(e) => &e.notes,
            Self::UndefinedType(e) => &e.notes,
            Self::UnificationFailure(e) => &e.notes,
            Self::FieldNotFound(e) => &e.notes,
            Self::NotARecord(e) => &e.notes,
            Self::NotAFunction(e) => &e.notes,
            Self::TypeAssertFailed(e) => &e.notes,
            Self::NonExhaustiveMatch(e) => &e.notes,
            Self::OverlappingInstancePatterns(e) => &e.notes,
            Self::ConsistencyViolation(e) => &e.notes,
            Self::CoverageViolation(e) => &e.notes,
            Self::InstanceContainsUnknown(e) => &e.notes,
            Self::KindMismatch(e) => &e.notes,
            Self::Generic(e) => &e.notes,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        let note = note.into();
        match &mut self {
            Self::ArityMismatch(e) => e.notes.push(note),
            Self::UndefinedVariable(e) => e.notes.push(note),
            Self::UndefinedType(e) => e.notes.push(note),
            Self::UnificationFailure(e) => e.notes.push(note),
            Self::FieldNotFound(e) => e.notes.push(note),
            Self::NotARecord(e) => e.notes.push(note),
            Self::NotAFunction(e) => e.notes.push(note),
            Self::TypeAssertFailed(e) => e.notes.push(note),
            Self::NonExhaustiveMatch(e) => e.notes.push(note),
            Self::OverlappingInstancePatterns(e) => e.notes.push(note),
            Self::ConsistencyViolation(e) => e.notes.push(note),
            Self::CoverageViolation(e) => e.notes.push(note),
            Self::InstanceContainsUnknown(e) => e.notes.push(note),
            Self::KindMismatch(e) => e.notes.push(note),
            Self::Generic(e) => e.notes.push(note),
        }
        self
    }

    /// Returns a stable kebab-case kind name for this error (used in unified error dicts).
    ///
    /// Unlike `code()` (which returns legacy T-codes), `kind_name()` returns a
    /// human-readable kebab-case identifier suitable for programmatic use in tinct code.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::ArityMismatch(_) => "arity-mismatch",
            Self::UndefinedVariable(_) => "undefined-variable",
            Self::UndefinedType(_) => "undefined-type",
            Self::UnificationFailure(_) => "unification-failure",
            Self::FieldNotFound(_) => "field-not-found",
            Self::NotARecord(_) => "not-a-dict",
            Self::NotAFunction(_) => "not-a-function",
            Self::TypeAssertFailed(_) => "type-assert-failed",
            Self::NonExhaustiveMatch(_) => "non-exhaustive-match",
            Self::OverlappingInstancePatterns(_) => "overlapping-instance-patterns",
            Self::ConsistencyViolation(_) => "consistency-violation",
            Self::CoverageViolation(_) => "coverage-violation",
            Self::InstanceContainsUnknown(_) => "instance-contains-unknown",
            Self::KindMismatch(_) => "kind-mismatch",
            Self::Generic(_) => "error",
        }
    }

    /// Returns the human-readable message for this error (without span or variant tag).
    ///
    /// This is the message component only — callers that need the full diagnostic
    /// format (with span, notes, source snippets) should use `format_type_error`.
    pub fn message(&self) -> String {
        match self {
            Self::ArityMismatch(e) => {
                let params_str = if e.params.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", e.params.join(", "))
                };
                let got_str = if e.got_types.is_empty() {
                    format!("{}", e.got)
                } else {
                    format!("{} [{}]", e.got, e.got_types.join(", "))
                };
                if let Some(ref name) = e.callee {
                    format!(
                        "`{}` expected {} arguments{}, got {}",
                        name, e.expected, params_str, got_str
                    )
                } else {
                    format!(
                        "expected {} arguments{}, got {}",
                        e.expected, params_str, got_str
                    )
                }
            }
            Self::UndefinedVariable(e) => format!("undefined variable: {}", e.name),
            Self::UndefinedType(e) => format!("undefined type: {}", e.name),
            Self::UnificationFailure(e) => format!("cannot unify {} with {}", e.expected, e.got),
            Self::FieldNotFound(e) => {
                format!("field '{}' not found in {}", e.field, e.record_type)
            }
            Self::NotARecord(e) => format!("expected Dict, got {}", e.actual),
            Self::NotAFunction(e) => {
                if let Some(ref name) = e.callee {
                    format!("expected `{}` to be a function, got {}", name, e.actual)
                } else {
                    format!("expected function type, got {}", e.actual)
                }
            }
            Self::TypeAssertFailed(e) => {
                format!(
                    "type assertion failed: expected {}, got {}",
                    e.asserted, e.actual
                )
            }
            Self::NonExhaustiveMatch(e) => {
                format!("non-exhaustive match: missing {}", e.missing.join(", "))
            }
            Self::OverlappingInstancePatterns(_) => "overlapping instance patterns".to_string(),
            Self::ConsistencyViolation(_) => "consistency violation".to_string(),
            Self::CoverageViolation(_) => "coverage violation".to_string(),
            Self::InstanceContainsUnknown(e) => e.message.clone(),
            Self::KindMismatch(e) => {
                format!("kind mismatch: expected `{}`, got {}", e.expected, e.actual)
            }
            Self::Generic(e) => e.message.clone(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────────
// Display
// ────────────────────────────────────────────────────────────────────────────────

impl fmt::Display for TypeErrorTyped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch(e) => {
                write!(
                    f,
                    "[ArityMismatch] expected {} arguments, got {}",
                    e.expected, e.got
                )
            }
            Self::UndefinedVariable(e) => {
                write!(f, "[UndefinedVariable] undefined variable: {}", e.name)
            }
            Self::UndefinedType(e) => {
                write!(f, "[UndefinedType] undefined type: {}", e.name)
            }
            Self::UnificationFailure(e) => {
                write!(
                    f,
                    "[UnificationFailure] cannot unify {} with {}",
                    e.expected, e.got
                )
            }
            Self::FieldNotFound(e) => {
                write!(
                    f,
                    "[FieldNotFound] field '{}' not found in {}",
                    e.field, e.record_type
                )
            }
            Self::NotARecord(e) => {
                write!(f, "[NotARecord] expected Dict, got {}", e.actual)
            }
            Self::NotAFunction(e) => {
                write!(f, "[NotAFunction] expected function type, got {}", e.actual)
            }
            Self::TypeAssertFailed(e) => {
                write!(
                    f,
                    "[TypeAssertFailed] type assertion failed: expected {}, got {}",
                    e.asserted, e.actual
                )
            }
            Self::NonExhaustiveMatch(e) => {
                write!(
                    f,
                    "[NonExhaustiveMatch] non-exhaustive match: missing {}",
                    e.missing.join(", ")
                )
            }
            Self::OverlappingInstancePatterns(_) => {
                write!(
                    f,
                    "[OverlappingInstancePatterns] overlapping instance patterns"
                )
            }
            Self::ConsistencyViolation(_) => {
                write!(f, "[ConsistencyViolation] consistency violation")
            }
            Self::CoverageViolation(_) => {
                write!(f, "[CoverageViolation] coverage violation")
            }
            Self::InstanceContainsUnknown(e) => {
                write!(f, "[InstanceContainsUnknown] {}", e.message)
            }
            Self::KindMismatch(e) => {
                write!(
                    f,
                    "[KindMismatch] kind mismatch: expected `{}`, got {}",
                    e.expected, e.actual
                )
            }
            Self::Generic(e) => {
                write!(f, "[TypeError] {}", e.message)
            }
        }
    }
}

impl std::error::Error for TypeErrorTyped {}

// ────────────────────────────────────────────────────────────────────────────────
// From impls
// ────────────────────────────────────────────────────────────────────────────────

impl From<ArityMismatch> for TypeErrorTyped {
    fn from(e: ArityMismatch) -> Self {
        TypeErrorTyped::ArityMismatch(e)
    }
}

impl From<UndefinedVariable> for TypeErrorTyped {
    fn from(e: UndefinedVariable) -> Self {
        TypeErrorTyped::UndefinedVariable(e)
    }
}

impl From<UndefinedType> for TypeErrorTyped {
    fn from(e: UndefinedType) -> Self {
        TypeErrorTyped::UndefinedType(e)
    }
}

impl From<UnificationFailure> for TypeErrorTyped {
    fn from(e: UnificationFailure) -> Self {
        TypeErrorTyped::UnificationFailure(e)
    }
}

impl From<FieldNotFound> for TypeErrorTyped {
    fn from(e: FieldNotFound) -> Self {
        TypeErrorTyped::FieldNotFound(e)
    }
}

impl From<NotARecord> for TypeErrorTyped {
    fn from(e: NotARecord) -> Self {
        TypeErrorTyped::NotARecord(e)
    }
}

impl From<NotAFunction> for TypeErrorTyped {
    fn from(e: NotAFunction) -> Self {
        TypeErrorTyped::NotAFunction(e)
    }
}

impl From<TypeAssertFailed> for TypeErrorTyped {
    fn from(e: TypeAssertFailed) -> Self {
        TypeErrorTyped::TypeAssertFailed(e)
    }
}

impl From<NonExhaustiveMatch> for TypeErrorTyped {
    fn from(e: NonExhaustiveMatch) -> Self {
        TypeErrorTyped::NonExhaustiveMatch(e)
    }
}

impl From<OverlappingInstancePatterns> for TypeErrorTyped {
    fn from(e: OverlappingInstancePatterns) -> Self {
        TypeErrorTyped::OverlappingInstancePatterns(e)
    }
}

impl From<ConsistencyViolation> for TypeErrorTyped {
    fn from(e: ConsistencyViolation) -> Self {
        TypeErrorTyped::ConsistencyViolation(e)
    }
}

impl From<CoverageViolation> for TypeErrorTyped {
    fn from(e: CoverageViolation) -> Self {
        TypeErrorTyped::CoverageViolation(e)
    }
}

impl From<InstanceContainsUnknown> for TypeErrorTyped {
    fn from(e: InstanceContainsUnknown) -> Self {
        TypeErrorTyped::InstanceContainsUnknown(e)
    }
}

impl From<KindMismatch> for TypeErrorTyped {
    fn from(e: KindMismatch) -> Self {
        TypeErrorTyped::KindMismatch(e)
    }
}

impl From<GenericTypeError> for TypeErrorTyped {
    fn from(e: GenericTypeError) -> Self {
        TypeErrorTyped::Generic(e)
    }
}
