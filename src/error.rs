//! Unified diagnostic system with definition-site spans, materialization-site spans, and stack frames.

use std::fmt;
use std::sync::Arc;

use crate::ast::Span;

/// Result with diagnostics attached to both success and failure paths.
///
/// `result` is the clear success/failure signal. `diagnostics` carries
/// warnings, notes, and errors regardless of outcome.
///
/// Fields are private. The only way to extract the Result is `take()`,
/// which pushes all diagnostics to the EvalContext. This makes it
/// impossible to forget diagnostic propagation.
#[must_use]
pub struct DiagnosticResult<T, E = Diagnostic> {
    result: Result<T, E>,
    diagnostics: Vec<Diagnostic>,
}

impl<T, E> DiagnosticResult<T, E> {
    pub fn ok(value: T) -> Self {
        Self {
            result: Ok(value),
            diagnostics: vec![],
        }
    }

    pub fn err(error: E) -> Self {
        Self {
            result: Err(error),
            diagnostics: vec![],
        }
    }

    pub fn with_diagnostics(mut self, diags: Vec<Diagnostic>) -> Self {
        self.diagnostics.extend(diags);
        self
    }

    /// Extract the Result, pushing all diagnostics to the collector.
    /// This is the only way to access the result.
    pub fn take(self, collector: &std::sync::Mutex<Vec<Diagnostic>>) -> Result<T, E> {
        collector
            .lock()
            .expect("diagnostic collector mutex poisoned")
            .extend(self.diagnostics);
        self.result
    }

    /// Destructure for test assertions. Not available in production code.
    #[cfg(test)]
    pub fn into_parts(self) -> (Result<T, E>, Vec<Diagnostic>) {
        (self.result, self.diagnostics)
    }

    /// Transform the success value, carrying diagnostics forward.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> DiagnosticResult<U, E> {
        DiagnosticResult {
            result: self.result.map(f),
            diagnostics: self.diagnostics,
        }
    }
}

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
/// Attached by the evaluator when a `CoreExpr::TypeAssert` fails at a pipeline boundary.
/// Carried through `TypeAssertCheckData::pipeline_blame` → `Cont::TypeAssertCheck` →
/// `Diagnostic::with_pipeline_blame`. Set to `None` for inline type assertions (`[@Type expr]`);
/// `Some` only when the TypeAssert was synthesized for a pipeline type-check call site.
///
/// Stage labels use document names when available (`--- name: %foo`), falling back to
/// `"document N"` for unnamed runtime documents (stage-skipped docs excluded).
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineBlame {
    /// The producing stage label (positive party — blamed for wrong output shape).
    /// E.g., "document 'data'" or "document 0".
    pub producer: String,
    /// The consuming stage label (negative party — blamed for wrong contract).
    /// E.g., "document 'transform'" or "document 1".
    pub consumer: Option<String>,
}

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

impl std::fmt::Display for DiagnosticLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnosticLevel::Info => write!(f, "info"),
            DiagnosticLevel::Warn => write!(f, "warning"),
            DiagnosticLevel::Err => write!(f, "error"),
        }
    }
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

/// Labeled span for error reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanLabel {
    pub span: crate::ast::Span,
    pub message: String,
}

/// Unified pipeline diagnostic — emitted by the parser, resolver, type checker, lowerer, and
/// evaluator. Used both as a Rust-internal diagnostic and as the shape of the tinct-side
/// `Diagnostic` type in `stdlib/builtin_core.llt`.
///
/// `Diagnostic` can have Info/Warn/Err level. Info diagnostics are used for structured trace
/// output (e.g., `trace-fn-type`, `trace-lower`, `trace-call`, `trace-return`). Warn
/// diagnostics are non-fatal notifications. Err diagnostics represent fatal pipeline errors.
///
/// At error sites, `Diagnostic` is frequently heap-allocated as `Box<Diagnostic>` to keep
/// `Result<T, Box<Diagnostic>>` pointer-sized.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub kind: &'static str,
    pub message: String,
    pub labels: Vec<SpanLabel>,
    pub notes: Vec<String>,
    pub children: Vec<Diagnostic>,
}

impl Diagnostic {
    /// Construct an error-level diagnostic with a single span and no notes.
    pub fn error(kind: &'static str, message: impl Into<String>, span: crate::ast::Span) -> Self {
        Self {
            level: DiagnosticLevel::Err,
            kind,
            message: message.into(),
            labels: vec![SpanLabel {
                span,
                message: String::new(),
            }],
            notes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Construct a warn-level diagnostic with a single span and no notes.
    pub fn warn(kind: &'static str, message: impl Into<String>, span: crate::ast::Span) -> Self {
        Self {
            level: DiagnosticLevel::Warn,
            kind,
            message: message.into(),
            labels: vec![SpanLabel {
                span,
                message: String::new(),
            }],
            notes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Construct an info-level diagnostic with a single span and no notes.
    pub fn info(kind: &'static str, message: impl Into<String>, span: crate::ast::Span) -> Self {
        Self {
            level: DiagnosticLevel::Info,
            kind,
            message: message.into(),
            labels: vec![SpanLabel {
                span,
                message: String::new(),
            }],
            notes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Construct a diagnostic with an explicit level and pre-built labels vec.
    pub fn with_spans(
        level: DiagnosticLevel,
        kind: &'static str,
        message: impl Into<String>,
        labels: Vec<SpanLabel>,
    ) -> Self {
        Self {
            level,
            kind,
            message: message.into(),
            labels,
            notes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Return a clone of this diagnostic with the level bumped one step (Info→Warn, Warn→Err, Err→Err).
    ///
    /// Used by `--strict` mode to promote warnings to errors.
    pub fn bump_level(&self) -> Self {
        let mut bumped = self.clone();
        bumped.level = bumped.level.bump();
        bumped
    }

    pub fn with_label(mut self, span: crate::ast::Span, message: impl Into<String>) -> Self {
        self.labels.push(SpanLabel {
            span,
            message: message.into(),
        });
        self
    }

    pub fn push_label(&mut self, span: crate::ast::Span, message: impl Into<String>) {
        self.labels.push(SpanLabel {
            span,
            message: message.into(),
        });
    }

    pub fn with_span(mut self, span: crate::ast::Span, label: impl Into<String>) -> Self {
        self.labels.push(SpanLabel {
            span,
            message: label.into(),
        });
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(self, help: impl Into<String>) -> Self {
        self.with_note(help)
    }

    pub fn add_help(&mut self, help: impl Into<String>) {
        self.notes.push(help.into());
    }

    pub fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn with_child(mut self, child: Diagnostic) -> Self {
        self.children.push(child);
        self
    }

    pub fn primary_span(&self) -> &crate::ast::Span {
        &self.labels[0].span
    }

    /// Wrap this diagnostic in a `Box` for use as `Err(diag.boxed())` in functions
    /// that return `Result<T, Box<Diagnostic>>`.
    pub fn boxed(self) -> Box<Self> {
        Box::new(self)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Diagnostic constructor compatibility methods
    // ─────────────────────────────────────────────────────────────────────

    /// Create an error with the Internal escape hatch kind.
    /// Use typed kind tags instead when possible.
    pub fn internal(message: String, span: Span) -> Self {
        Self::error("internal-error", message, span)
    }

    /// Create a BuilderFinished error for a specific operation.
    /// Used when a builder operation is attempted after builder-finish.
    pub fn builder_already_finished(op: impl Into<String>, span: Span) -> Self {
        Self::error(
            "builder-finished",
            format!("{}: builder has already been finished", op.into()),
            span,
        )
    }

    /// Attach a materialization span (where this thunk was forced).
    /// Appended as a label with text `"evaluated here"`.
    pub fn with_materialization_span(self, span: Span) -> Self {
        self.with_label(span, "evaluated here")
    }

    /// Builder for stack frame attachment.
    pub fn with_frame(self, label: String, span: Span) -> Self {
        self.with_label(span, label)
    }

    /// Mutable stack frame push.
    pub fn push_frame(&mut self, label: String, span: Span) {
        self.push_label(span, label);
    }

    /// Attach a secondary span with a label, e.g. `"value produced here"`.
    /// Displayed on a separate line. Multiple calls append; all labels are displayed in order.
    pub fn with_secondary_span(self, span: Span, label: impl Into<String>) -> Self {
        self.with_label(span, label)
    }

    /// Builder for attaching macro expansion provenance.
    /// Shows "in expansion of `<name>` at line:col" in the error output.
    pub fn with_macro_expansion(self, macro_name: String, call_site: Span) -> Self {
        self.with_child(Diagnostic::info(
            "macro-expansion",
            format!("in expansion of '{}'", macro_name),
            call_site,
        ))
    }

    /// Attach a blame label for gradual typing boundary errors.
    /// Uses co-natural strategy: innermost blame label is preserved, outer labels discarded.
    pub fn with_blame(mut self, label: BlameLabel) -> Self {
        // Co-natural strategy: if we already have a blame label in notes (innermost boundary),
        // keep it and discard the new outer label.
        let blame_marker = "blame:";
        if !self.notes.iter().any(|n| n.starts_with(blame_marker)) {
            let party = match label.polarity {
                BlameParity::Positive => "typed side (annotation)",
                BlameParity::Negative => "untyped side (producer)",
            };
            self = self
                .with_label(label.origin_span, "value originates here")
                .with_label(
                    label.boundary_span,
                    format!("typed/untyped boundary ({})", party),
                );
            self.notes.push(format!("blame: {} responsible", party));
        }
        self
    }

    /// Attach pipeline blame provenance for contract violation errors.
    /// Identifies the producing stage (positive party) and consuming stage (negative party).
    pub fn with_pipeline_blame(mut self, blame: PipelineBlame) -> Self {
        // Only attach if not already present
        if !self.notes.iter().any(|n| n.starts_with("produced by:")) {
            self.notes.push(format!("produced by: {}", blame.producer));
            if let Some(consumer) = blame.consumer {
                self.notes.push(format!("consumed by: {}", consumer));
            }
        }
        self
    }

    pub fn key_not_found(key: &str, available_keys: Vec<String>, span: Span) -> Self {
        let mut diag = Self::error("key-not-found", format!("key not found: {}", key), span);
        if !available_keys.is_empty() {
            // Find best match using Jaro-Winkler similarity.
            // Threshold 0.8 is consistent with prior art (cargo, rustc suggest at ~0.8).
            let mut best_match: Option<(&str, f64)> = None;
            for avail in &available_keys {
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
                diag.notes.push(format!("did you mean: '{}'?", suggestion));
            } else {
                // No close match found above the similarity threshold.
                // Fall back to listing up to 5 available keys.
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
                diag.notes
                    .push(format!("available keys: {}{}", keys_str, suffix));
            }
        }
        diag
    }

    pub fn type_mismatch(expected: impl AsRef<str>, got: impl AsRef<str>, span: Span) -> Self {
        Self::error(
            "type-mismatch",
            format!(
                "type mismatch: expected {}, got {}",
                expected.as_ref(),
                got.as_ref()
            ),
            span,
        )
    }

    pub fn arity_mismatch(expected: usize, got: usize, span: Span) -> Self {
        Self::error(
            "arity-mismatch",
            format!(
                "arity mismatch: expected {}, got {}",
                ArityBound::Exact(expected),
                got
            ),
            span,
        )
    }

    pub fn arity_mismatch_bound(expected: ArityBound, got: usize, span: Span) -> Self {
        Self::error(
            "arity-mismatch",
            format!("arity mismatch: expected {}, got {}", expected, got),
            span,
        )
    }

    /// Attach a callee name to call-related errors (arity, missing param, named arg issues).
    /// Appends to notes.
    pub fn set_arity_callee(&mut self, callee: Option<Arc<str>>) {
        if let Some(name) = callee {
            if !self.notes.iter().any(|n| n.starts_with("in call to")) {
                self.notes.push(format!("in call to '{}'", name));
            }
        }
    }

    /// Attach parameter names to an arity mismatch error.
    /// Appends to notes.
    pub fn set_arity_params(&mut self, params: Vec<String>) {
        if !params.is_empty() && !self.notes.iter().any(|n| n.starts_with("parameters:")) {
            self.notes
                .push(format!("parameters: {}", params.join(", ")));
        }
    }

    pub fn circular_dependency(name: &str, span: Span, cycle_path: Vec<(Arc<str>, Span)>) -> Self {
        let mut diag = Self::error(
            "circular-dependency",
            format!("circular dependency detected while evaluating {}", name),
            span,
        );
        if !cycle_path.is_empty() {
            diag.notes.push("cycle:".to_string());
            for (label, span) in cycle_path {
                diag.notes.push(format!("  {} at {}", label, span));
            }
            diag.notes.push(format!("  → back to {}", name));
        }
        diag
    }

    pub fn match_exhaustion(scrutinee_type: &str, span: Span) -> Self {
        Self::error(
            "match-exhaustion",
            format!(
                "non-exhaustive match: no pattern matched the {} value",
                scrutinee_type
            ),
            span,
        )
    }

    pub fn duplicate_variable_in_pattern(name: &str, span: Span) -> Self {
        Self::error(
            "duplicate-variable",
            format!(
                "duplicate variable in pattern: '{}' appears more than once",
                name
            ),
            span,
        )
    }

    pub fn user_error(message: String, span: Span) -> Self {
        Self::error("user-error", message, span)
    }

    pub fn macro_error(message: String, span: Span) -> Self {
        Self::error(
            "macro-error",
            format!("macro expansion error: {}", message),
            span,
        )
    }

    pub fn unimplemented(message: String, span: Span) -> Self {
        Self::error("unimplemented", message, span)
    }

    pub fn schema_violation(violations: Vec<(String, String)>, span: Span) -> Self {
        let mut diag = Self::error(
            "schema-violation",
            format!(
                "schema validation failed with {} error(s)",
                violations.len()
            ),
            span,
        );
        for (field, msg) in violations {
            diag.notes.push(format!("{}: {}", field, msg));
        }
        diag
    }

    pub fn kind_mismatch(expected: &str, got: &str, span: Span) -> Self {
        Self::error(
            "kind-mismatch",
            format!("kind mismatch: expected {}, got {}", expected, got),
            span,
        )
    }

    pub fn named_arg_rejected(builtin: String, span: Span) -> Self {
        Self::error(
            "named-arg-rejected",
            format!("{} does not accept named arguments", builtin),
            span,
        )
    }

    pub fn integer_overflow(op: String, span: Span) -> Self {
        Self::error(
            "integer-overflow",
            format!("{}: integer overflow", op),
            span,
        )
    }

    pub fn type_mismatch_ctx(
        context: String,
        expected: impl AsRef<str>,
        got: impl AsRef<str>,
        span: Span,
    ) -> Self {
        Self::error(
            "type-mismatch",
            format!(
                "{}: expected {}, got {}",
                context,
                expected.as_ref(),
                got.as_ref()
            ),
            span,
        )
    }

    pub fn division_by_zero(op: String, span: Span) -> Self {
        Self::error(
            "division-by-zero",
            format!("{}: division by zero", op),
            span,
        )
    }

    pub fn float_not_finite(builtin: String, value: f64, span: Span) -> Self {
        Self::error(
            "float-not-finite",
            format!("{}: {} is not a finite number", builtin, value),
            span,
        )
    }

    pub fn empty_collection(op: String, span: Span) -> Self {
        Self::error(
            "empty-collection",
            format!("{} on empty collection", op),
            span,
        )
    }

    pub fn value_not_serializable(value_type: String, span: Span) -> Self {
        Self::error(
            "value-not-serializable",
            format!("cannot serialize {} to JSON", value_type),
            span,
        )
    }

    pub fn float_out_of_range(builtin: String, value: f64, span: Span) -> Self {
        Self::error(
            "float-out-of-range",
            format!("{}: {} is out of range for Int", builtin, value),
            span,
        )
    }

    pub fn undefined_variable(name: String, span: Span) -> Self {
        Self::error(
            "undefined-variable",
            format!("undefined variable: {}", name),
            span,
        )
    }

    pub fn type_assert_failed(expected: &str, got: &str, span: Span) -> Self {
        Self::error(
            "type-assert-failed",
            format!("type assertion failed: expected {}, got {}", expected, got),
            span,
        )
    }

    pub fn no_instance(class_name: &str, type_tags: Vec<String>, span: Span) -> Self {
        let types_str = type_tags.join(", ");
        Self::error(
            "no-instance",
            format!("no instance for {} ({})", class_name, types_str),
            span,
        )
    }

    pub fn named_arg_conflict(param: String, span: Span) -> Self {
        Self::error(
            "named-arg-conflict",
            format!(
                "parameter '{}' received both positional and named argument",
                param
            ),
            span,
        )
    }

    pub fn unknown_named_arg(name: String, valid_params: Vec<String>, span: Span) -> Self {
        let msg = if valid_params.is_empty() {
            format!(
                "unexpected named argument: \"{}\" (function has no parameters)",
                name
            )
        } else {
            let valid = valid_params.join(", ");
            format!(
                "unexpected named argument: \"{}\" (valid parameter names: {})",
                name, valid
            )
        };
        Self::error("unknown-named-arg", msg, span)
    }

    pub fn duplicate_key(key: &str, span: Span) -> Self {
        Self::error("duplicate-key", format!("duplicate key: {}", key), span)
    }

    pub fn resource_limit_exceeded(message: impl Into<String>, span: Span) -> Self {
        Self::error("resource-limit", message, span)
    }

    pub fn include_io_error(path: String, detail: String, span: Span) -> Self {
        Self::error(
            "include-io-error",
            format!("include: cannot access \"{}\": {}", path, detail),
            span,
        )
    }

    pub fn include_cycle(path: String, span: Span) -> Self {
        Self::error(
            "include-cycle",
            format!("circular include detected: \"{}\"", path),
            span,
        )
    }

    pub fn include_file_too_large(path: String, size: u64, limit: u64, span: Span) -> Self {
        Self::error(
            "include-file-too-large",
            format!(
                "include: file \"{}\" is {} bytes, exceeds {} byte limit",
                path, size, limit
            ),
            span,
        )
    }

    pub fn include_hash_mismatch(
        path: String,
        expected: String,
        actual: String,
        span: Span,
    ) -> Self {
        Self::error(
            "include-hash-mismatch",
            format!(
                "include: integrity check failed for \"{}\": expected {}, got {}",
                path, expected, actual
            ),
            span,
        )
    }

    pub fn include_hash_required(path: String, span: Span) -> Self {
        Self::error(
            "include-hash-required",
            format!(
                "include: integrity hash required for \"{}\" (--require-integrity)",
                path
            ),
            span,
        )
    }

    pub fn parse_conversion(builtin: String, input: String, target: &str, span: Span) -> Self {
        Self::error(
            "parse-conversion",
            format!("{}: cannot parse {:?} as {}", builtin, input, target),
            span,
        )
    }

    pub fn uri_parse_error(detail: String, span: Span) -> Self {
        Self::error(
            "uri-parse-error",
            format!("URI parse error: {}", detail),
            span,
        )
    }

    pub fn missing_required_param(param: impl Into<String>, span: Span) -> Self {
        Self::error(
            "missing-required-param",
            format!("missing argument for required parameter '{}'", param.into()),
            span,
        )
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header: level[kind]: message
        write!(f, "{}[{}]: {}", self.level, self.kind, self.message)?;

        // Primary span location
        if let Some(primary) = self.labels.first() {
            write!(
                f,
                "\n --> {}:{}:{}",
                primary.span.file, primary.span.start_line, primary.span.start_col
            )?;
            if !primary.message.is_empty() {
                write!(f, "  {}", primary.message)?;
            }
        }

        // Secondary labeled spans
        for label in self.labels.iter().skip(1) {
            write!(
                f,
                "\n --> {}:{}:{}",
                label.span.file, label.span.start_line, label.span.start_col
            )?;
            if !label.message.is_empty() {
                write!(f, "  {}", label.message)?;
            }
        }

        // Text-only notes
        for note in &self.notes {
            write!(f, "\n  = note: {}", note)?;
        }

        // Child diagnostics — indented recursively
        for child in &self.children {
            let child_str = format!("{}", child);
            for line in child_str.lines() {
                write!(f, "\n  {}", line)?;
            }
        }

        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

pub fn has_type_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.level == DiagnosticLevel::Err)
}

/// Format a Diagnostic with source context snippets.
///
/// Enhances the base Display format with source code snippets around each span.
/// `source` is the source text to extract snippets from. If a span's file doesn't
/// match the provided source (e.g., Rust-side spans from `rust_span!()`), the
/// snippet is omitted and the base Display location is shown.
pub fn format_diagnostic(diag: &Diagnostic, source: &str, _file_name: &str) -> String {
    let mut out = format!("{}[{}]: {}", diag.level, diag.kind, diag.message);

    for (i, label) in diag.labels.iter().enumerate() {
        let prefix = if i == 0 { " at" } else { "-->" };
        out.push_str(&format!(
            "\n {prefix} {}:{}:{}",
            label.span.file, label.span.start_line, label.span.start_col
        ));
        if let Some(snippet) = render_span_snippet(source, label.span.clone()) {
            out.push_str("\n  |\n");
            out.push_str(&snippet);
            if !label.message.is_empty() {
                let trimmed = out.trim_end_matches('\n');
                out = format!("{}  {}\n", trimmed, label.message);
            }
        } else if !label.message.is_empty() {
            if i == 0 {
                out.push_str(&format!("  {}", label.message));
            } else {
                out.push_str(&format!("  {}", label.message));
            }
        }
    }

    for note in &diag.notes {
        out.push_str(&format!("\n  = note: {note}"));
    }

    for child in &diag.children {
        let child_str = format_diagnostic(child, source, _file_name);
        for line in child_str.lines() {
            out.push_str(&format!("\n  {}", line));
        }
    }

    out.push('\n');
    out
}

/// Render a source snippet with caret annotations for the given span.
///
/// Returns `None` if the span's line/offset is out of bounds for the provided source text
/// (e.g. spans from Rust source via `rust_span!()`, which refer to Rust files not user source).
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
    // Split source into lines
    let lines: Vec<&str> = source.lines().collect();

    // Convert position fields to usize for indexing.
    let start_line = span.start_line as usize;
    let end_line_raw = span.end_line as usize;
    let start_col_raw = span.start_col as usize;
    let end_col_raw = span.end_col as usize;

    // Span uses 1-based line numbers
    if start_line < 1 || start_line > lines.len() {
        return None; // Span out of bounds
    }

    // Width of the largest line number shown, for consistent gutter alignment.
    let end_line = end_line_raw.min(lines.len());
    let line_num_width = end_line.to_string().len();
    let padding = " ".repeat(line_num_width);

    let mut snippet = String::new();

    if start_line == end_line {
        // ── Single-line span ──────────────────────────────────────────────
        let line_text = lines[start_line - 1];
        let start_col = start_col_raw.saturating_sub(1).min(line_text.len());
        let end_col = end_col_raw.saturating_sub(1).min(line_text.len());
        let caret_length = if end_col > start_col {
            end_col - start_col
        } else {
            1
        };

        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            start_line,
            line_text,
            width = line_num_width
        ));
        snippet.push_str(&format!("  {} | ", padding));
        snippet.push_str(&" ".repeat(start_col));
        snippet.push_str(&"^".repeat(caret_length));
    } else {
        // ── Multi-line span ───────────────────────────────────────────────
        let first_line = lines[start_line - 1];
        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            start_line,
            first_line,
            width = line_num_width
        ));

        // Middle lines: show full line content (no caret yet).
        for line_num in (start_line + 1)..end_line {
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
        let end_col = end_col_raw.saturating_sub(1).min(last_line.len());
        snippet.push_str(&format!(
            "  {:>width$} | {}\n",
            end_line,
            last_line,
            width = line_num_width
        ));

        // Caret line: underline the last line from col 0 to end_col.
        let caret_length = end_col.max(1);
        snippet.push_str(&format!("  {} | ", padding));
        snippet.push_str(&"^".repeat(caret_length));
    }

    Some(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_span;
    use crate::test_util::test_span;

    #[test]
    fn test_diagnostic_new() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::internal("something broke".to_string(), span.clone());
        assert_eq!(err.message, "something broke");
        assert_eq!(err.labels[0].span, span);
        assert_eq!(err.labels[0].message, "");
        assert_eq!(err.labels.len(), 1);
    }

    #[test]
    fn test_diagnostic_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err = Diagnostic::internal("lazy fail".to_string(), def_span)
            .with_materialization_span(mat_span.clone());
        assert_eq!(err.labels.len(), 2);
        assert_eq!(err.labels[1].span, mat_span);
        assert_eq!(err.labels[1].message, "evaluated here");
    }

    #[test]
    fn test_diagnostic_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = Diagnostic::internal("err".to_string(), span)
            .with_frame("my_function".to_string(), frame_span.clone());
        assert_eq!(err.labels.len(), 2);
        assert_eq!(err.labels[1].message, "my_function");
        assert_eq!(err.labels[1].span, frame_span);
    }

    #[test]
    fn test_diagnostic_key_not_found() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::key_not_found("foo", vec![], span);
        assert_eq!(err.message, "key not found: foo");
    }

    #[test]
    fn test_diagnostic_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::type_mismatch("Int", "String", span);
        assert_eq!(err.message, "type mismatch: expected Int, got String");
    }

    #[test]
    fn test_diagnostic_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::arity_mismatch(2, 3, span);
        assert_eq!(err.message, "arity mismatch: expected 2 arguments, got 3");
    }

    #[test]
    fn test_diagnostic_circular_dependency() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::circular_dependency("x", span, Vec::new());
        assert_eq!(
            err.message,
            "circular dependency detected while evaluating x"
        );
    }

    #[test]
    fn test_circular_dependency_cycle_path_format() {
        let def_span = test_span(1, 1, 1, 5);
        let path = vec![
            (std::sync::Arc::from("[f ...]"), test_span(10, 1, 10, 8)),
            (std::sync::Arc::from("[g ...]"), test_span(20, 3, 20, 10)),
        ];
        let err = Diagnostic::circular_dependency("x", def_span, path);
        // Check that cycle notes are present
        assert!(err.notes.iter().any(|n| n.contains("[f ...]")));
        assert!(err.notes.iter().any(|n| n.contains("[g ...]")));
        assert!(err.notes.iter().any(|n| n.contains("→ back to x")));
    }

    #[test]
    fn test_diagnostic_display_basic() {
        let span = test_span(3, 5, 3, 10);
        let err = Diagnostic::internal("oops".to_string(), span);
        let display = format!("{err}");
        // Format is now "line:col: message"
        assert_eq!(
            display,
            "error[internal-error]: oops\n --> src/test_util.rs:3:5"
        );
    }

    #[test]
    fn test_diagnostic_display_full() {
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let frame1_span = test_span(10, 2, 10, 8);
        let frame2_span = test_span(15, 1, 15, 12);
        let err = Diagnostic::internal("bad value".to_string(), def_span)
            .with_materialization_span(mat_span)
            .with_frame("outer".to_string(), frame1_span)
            .with_frame("inner".to_string(), frame2_span);
        let display = format!("{err}");
        assert!(
            display.contains("error[internal-error]: bad value"),
            "got: {display}"
        );
        assert!(
            display.contains("--> src/test_util.rs:3:5"),
            "got: {display}"
        );
        assert!(
            display.contains("--> src/test_util.rs:20:1"),
            "got: {display}"
        );
        assert!(display.contains("evaluated here"), "got: {display}");
        assert!(display.contains("outer"), "got: {display}");
        assert!(display.contains("inner"), "got: {display}");
    }

    #[test]
    fn test_diagnostic_push_frame() {
        let span = test_span(1, 1, 1, 5);
        let mut err = Diagnostic::internal("error".to_string(), span);

        assert_eq!(err.labels.len(), 1); // Only primary

        // Push a frame directly
        let frame_span = test_span(5, 1, 5, 10);
        err.push_frame("first_function".to_string(), frame_span.clone());

        assert_eq!(err.labels.len(), 2);
        assert_eq!(err.labels[1].message, "first_function");
        assert_eq!(err.labels[1].span, frame_span);

        // Push a second frame
        let frame2_span = test_span(10, 3, 10, 15);
        err.push_frame("second_function".to_string(), frame2_span.clone());

        assert_eq!(err.labels.len(), 3);
        assert_eq!(err.labels[2].message, "second_function");
        assert_eq!(err.labels[2].span, frame2_span);
    }

    #[test]
    fn test_arity_bound_display() {
        assert_eq!(format!("{}", ArityBound::Exact(1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Exact(2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 0)), "0 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 1)), "1 argument");
        assert_eq!(format!("{}", ArityBound::Range(2, 2)), "2 arguments");
        assert_eq!(format!("{}", ArityBound::Range(1, 3)), "1 to 3 arguments");
        assert_eq!(format!("{}", ArityBound::Range(0, 5)), "0 to 5 arguments");
    }

    #[test]
    fn test_with_materialization_span_label() {
        let span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 1, 10, 5);
        let err = Diagnostic::internal("err".to_string(), span)
            .with_materialization_span(mat_span.clone());
        assert_eq!(err.labels[1].span, mat_span);
        assert_eq!(err.labels[1].message, "evaluated here");
    }

    #[test]
    fn test_key_not_found_with_suggestion() {
        let err = Diagnostic::key_not_found(
            "nme",
            vec!["name".to_string(), "age".to_string()],
            test_span(1, 1, 1, 5),
        );
        assert!(err.message.contains("key not found: nme"));
        assert!(err.notes.iter().any(|n| n.contains("did you mean: 'name'")));
    }

    #[test]
    fn test_key_not_found_with_available_keys() {
        let err = Diagnostic::key_not_found(
            "xyz",
            vec!["name".to_string(), "age".to_string(), "address".to_string()],
            test_span(1, 1, 1, 5),
        );
        assert!(err.message.contains("key not found: xyz"));
        assert!(err.notes.iter().any(|n| n.contains("available keys:")));
        assert!(err.notes.iter().any(|n| n.contains("name")));
        assert!(err.notes.iter().any(|n| n.contains("age")));
        assert!(err.notes.iter().any(|n| n.contains("address")));
    }

    #[test]
    fn test_key_not_found_truncates_long_list() {
        let err = Diagnostic::key_not_found(
            "missing",
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f".to_string(),
                "g".to_string(),
            ],
            test_span(1, 1, 1, 5),
        );
        let note = err
            .notes
            .iter()
            .find(|n| n.contains("available keys:"))
            .unwrap();
        assert!(note.contains("..."));
        assert!(note.contains("a, b, c, d, e"));
        assert!(!note.contains(", f"));
    }

    #[test]
    fn test_key_not_found_empty_available_keys() {
        let err = Diagnostic::key_not_found("foo", vec![], test_span(1, 1, 1, 5));
        assert_eq!(err.message, "key not found: foo");
        assert!(err.notes.is_empty());
    }

    #[test]
    fn test_diagnostic_level_bump() {
        assert_eq!(DiagnosticLevel::Info.bump(), DiagnosticLevel::Warn);
        assert_eq!(DiagnosticLevel::Warn.bump(), DiagnosticLevel::Err);
        assert_eq!(DiagnosticLevel::Err.bump(), DiagnosticLevel::Err);
    }

    #[test]
    fn test_diagnostic_level_equality() {
        assert_eq!(DiagnosticLevel::Info, DiagnosticLevel::Info);
        assert_eq!(DiagnosticLevel::Warn, DiagnosticLevel::Warn);
        assert_eq!(DiagnosticLevel::Err, DiagnosticLevel::Err);
        assert_ne!(DiagnosticLevel::Info, DiagnosticLevel::Warn);
        assert_ne!(DiagnosticLevel::Warn, DiagnosticLevel::Err);
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
    fn test_diagnostic_with_blame() {
        let def_span = test_span(7, 1, 7, 5);
        let err = Diagnostic::type_assert_failed("Int", "String", def_span);

        let origin_span = test_span(3, 5, 3, 10);
        let boundary_span = test_span(7, 1, 7, 15);
        let label = BlameLabel {
            origin_span,
            boundary_span,
            polarity: BlameParity::Positive,
        };

        let err_with_blame = err.with_blame(label);
        // Verify labels were added
        assert!(err_with_blame.labels.len() > 2);
        assert!(err_with_blame
            .labels
            .iter()
            .any(|l| l.message == "value originates here"));
        assert!(err_with_blame
            .labels
            .iter()
            .any(|l| l.message.contains("typed/untyped boundary")));
    }

    #[test]
    fn test_blame_co_natural_strategy() {
        let def_span = test_span(10, 1, 10, 5);
        let err = Diagnostic::type_assert_failed("Int", "String", def_span);

        let inner_label = BlameLabel {
            origin_span: test_span(5, 1, 5, 10),
            boundary_span: test_span(8, 1, 8, 15),
            polarity: BlameParity::Positive,
        };

        let outer_label = BlameLabel {
            origin_span: test_span(1, 1, 1, 5),
            boundary_span: test_span(12, 1, 12, 20),
            polarity: BlameParity::Negative,
        };

        let err = err.with_blame(inner_label);
        let inner_label_count = err.labels.len();
        // Apply outer label (should be discarded)
        let err = err.with_blame(outer_label);
        // Should have same number of labels
        assert_eq!(err.labels.len(), inner_label_count);
    }

    #[test]
    fn test_render_span_snippet_single_line() {
        let source = "line1\nlet x = 42\nline3";
        let span = Span {
            file: rust_span!().file,
            start_line: 2,
            start_col: 1,
            end_line: 2,
            end_col: 4,
            name: None,
        };
        let snippet = render_span_snippet(source, span).unwrap();

        assert!(snippet.contains("2 | let x = 42"));
        assert!(snippet.contains("  | ^^^"));
        assert!(!snippet.contains("..."));
    }

    #[test]
    fn test_render_span_snippet_no_source_for_rust_spans() {
        let source = "test source";
        let snippet = render_span_snippet(source, rust_span!());
        assert_eq!(snippet, None);
    }

    #[test]
    fn test_render_span_snippet_multiline() {
        let source = "line1\nlet x = [\n  42\n]\nline5";
        let span = Span {
            file: rust_span!().file,
            start_line: 2,
            start_col: 1,
            end_line: 4,
            end_col: 2,
            name: None,
        };
        let snippet = render_span_snippet(source, span).unwrap();

        assert!(
            snippet.contains("2 | let x = ["),
            "missing first line: {snippet}"
        );
        assert!(
            snippet.contains("3 |   42"),
            "missing middle line: {snippet}"
        );
        assert!(snippet.contains("4 | ]"), "missing last line: {snippet}");
        assert!(snippet.contains('^'), "missing caret: {snippet}");
        assert!(
            !snippet.contains("..."),
            "unexpected '...' in snippet: {snippet}"
        );
    }

    /// Helper: build a Span with a given file path.
    fn span_with_file(path: &str) -> Span {
        Span {
            file: Arc::from(path),
            start_line: 3,
            start_col: 5,
            end_line: 3,
            end_col: 10,
            name: None,
        }
    }

    #[test]
    fn test_span_display_with_real_file() {
        let span = span_with_file("prelude.llt");
        assert_eq!(format!("{span}"), "prelude.llt:3:5-3:10");
    }

    #[test]
    fn test_span_display_with_synthetic_file() {
        let span = span_with_file("<parse>");
        assert_eq!(format!("{span}"), "3:5-3:10");
    }

    #[test]
    fn test_span_display_rust_file() {
        let span = rust_span!();
        let s = format!("{span}");
        assert!(
            s.contains("src/"),
            "rust span must include file path, got: {s}"
        );
    }

    #[test]
    fn test_display_error_with_file_in_definition_span() {
        let span = span_with_file("prelude.llt");
        let err = Diagnostic::internal("oops".to_string(), span);
        let display = format!("{err}");
        assert_eq!(display, "error[internal-error]: oops\n --> prelude.llt:3:5");
    }

    #[test]
    fn test_display_error_rust_span_no_panic() {
        let err = Diagnostic::internal("synthetic error".to_string(), rust_span!());
        let display = format!("{err}");
        assert!(
            display.contains("synthetic error"),
            "display must include the message; got: {display}"
        );
    }
}
