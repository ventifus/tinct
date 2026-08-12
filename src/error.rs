//! Unified diagnostic system with definition-site spans, materialization-site spans, and stack frames.

use std::fmt;

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
    use std::sync::Arc;

    #[test]
    fn test_diagnostic_new() {
        let span = test_span(1, 1, 1, 5);
        let err = Diagnostic::error("test", "something broke", span.clone());
        assert_eq!(err.message, "something broke");
        assert_eq!(err.labels[0].span, span);
        assert_eq!(err.labels[0].message, "");
        assert_eq!(err.labels.len(), 1);
    }

    #[test]
    fn test_diagnostic_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err = Diagnostic::error("test", "lazy fail", def_span)
            .with_materialization_span(mat_span.clone());
        assert_eq!(err.labels.len(), 2);
        assert_eq!(err.labels[1].span, mat_span);
        assert_eq!(err.labels[1].message, "evaluated here");
    }

    #[test]
    fn test_diagnostic_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = Diagnostic::error("test", "err", span)
            .with_frame("my_function".to_string(), frame_span.clone());
        assert_eq!(err.labels.len(), 2);
        assert_eq!(err.labels[1].message, "my_function");
        assert_eq!(err.labels[1].span, frame_span);
    }

    #[test]
    fn test_diagnostic_display_basic() {
        let span = test_span(3, 5, 3, 10);
        let err = Diagnostic::error("test", "oops", span);
        let display = format!("{err}");
        assert_eq!(display, "error[test]: oops\n --> src/test_util.rs:3:5");
    }

    #[test]
    fn test_diagnostic_display_full() {
        let def_span = test_span(3, 5, 3, 10);
        let mat_span = test_span(20, 1, 20, 5);
        let frame1_span = test_span(10, 2, 10, 8);
        let frame2_span = test_span(15, 1, 15, 12);
        let err = Diagnostic::error("test", "bad value", def_span)
            .with_materialization_span(mat_span)
            .with_frame("outer".to_string(), frame1_span)
            .with_frame("inner".to_string(), frame2_span);
        let display = format!("{err}");
        assert!(display.contains("error[test]: bad value"), "got: {display}");
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
        let mut err = Diagnostic::error("test", "error", span);

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
        let err = Diagnostic::error("test", "oops", span);
        let display = format!("{err}");
        assert_eq!(display, "error[test]: oops\n --> prelude.llt:3:5");
    }

    #[test]
    fn test_display_error_rust_span_no_panic() {
        let err = Diagnostic::error("test", "synthetic error", rust_span!());
        let display = format!("{err}");
        assert!(
            display.contains("synthetic error"),
            "display must include the message; got: {display}"
        );
    }
}
