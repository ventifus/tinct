// Evaluator foundation -- used starting Phase 1b
#![allow(dead_code)]

use std::fmt;

use crate::ast::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    pub label: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub message: String,
    pub definition_span: Span,
    pub materialization_span: Option<Span>,
    pub stack: Vec<StackFrame>,
}

impl EvalError {
    pub fn new(message: impl Into<String>, definition_span: Span) -> Self {
        Self {
            message: message.into(),
            definition_span,
            materialization_span: None,
            stack: Vec::new(),
        }
    }

    pub fn with_materialization_span(mut self, span: Span) -> Self {
        self.materialization_span = Some(span);
        self
    }

    pub fn with_frame(mut self, label: impl Into<String>, span: Span) -> Self {
        self.stack.push(StackFrame {
            label: label.into(),
            span,
        });
        self
    }

    pub fn push_frame(&mut self, label: impl Into<String>, span: Span) {
        self.stack.push(StackFrame {
            label: label.into(),
            span,
        });
    }

    pub fn key_not_found(key: &str, definition_span: Span) -> Self {
        Self::new(format!("key not found: {key}"), definition_span)
    }

    pub fn type_mismatch(expected: &str, got: &str, definition_span: Span) -> Self {
        Self::new(
            format!("type mismatch: expected {expected}, got {got}"),
            definition_span,
        )
    }

    pub fn arity_mismatch(expected: usize, got: usize, definition_span: Span) -> Self {
        Self::new(
            format!("arity mismatch: expected {expected} arguments, got {got}"),
            definition_span,
        )
    }

    pub fn circular_dependency(name: &str, definition_span: Span) -> Self {
        Self::new(
            format!("circular dependency detected while evaluating {name}"),
            definition_span,
        )
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (defined at {})", self.message, self.definition_span)?;
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
    use crate::ast::Position;

    fn test_span(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> Span {
        Span::new(
            Position {
                offset: 0,
                line: start_line,
                column: start_col,
            },
            Position {
                offset: 0,
                line: end_line,
                column: end_col,
            },
        )
    }

    #[test]
    fn test_eval_error_new() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::new("something broke", span.clone());
        assert_eq!(err.message, "something broke");
        assert_eq!(err.definition_span, span);
        assert_eq!(err.materialization_span, None);
        assert!(err.stack.is_empty());
    }

    #[test]
    fn test_eval_error_with_materialization_span() {
        let def_span = test_span(1, 1, 1, 5);
        let mat_span = test_span(10, 3, 10, 8);
        let err = EvalError::new("lazy fail", def_span).with_materialization_span(mat_span.clone());
        assert_eq!(err.materialization_span, Some(mat_span));
    }

    #[test]
    fn test_eval_error_with_frame() {
        let span = test_span(1, 1, 1, 5);
        let frame_span = test_span(5, 1, 5, 10);
        let err = EvalError::new("err", span).with_frame("my_function", frame_span.clone());
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "my_function");
        assert_eq!(err.stack[0].span, frame_span);
    }

    #[test]
    fn test_eval_error_key_not_found() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::key_not_found("foo", span);
        assert_eq!(err.message, "key not found: foo");
    }

    #[test]
    fn test_eval_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::type_mismatch("Int", "String", span);
        assert_eq!(err.message, "type mismatch: expected Int, got String");
    }

    #[test]
    fn test_eval_error_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::arity_mismatch(2, 3, span);
        assert_eq!(err.message, "arity mismatch: expected 2 arguments, got 3");
    }

    #[test]
    fn test_eval_error_circular_dependency() {
        let span = test_span(1, 1, 1, 5);
        let err = EvalError::circular_dependency("$x", span);
        assert_eq!(
            err.message,
            "circular dependency detected while evaluating $x"
        );
    }

    #[test]
    fn test_eval_error_display_basic() {
        let span = test_span(3, 5, 3, 10);
        let err = EvalError::new("oops", span);
        let display = format!("{err}");
        assert_eq!(display, "oops (defined at 3:5-3:10)");
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
bad value (defined at 3:5-3:10) (materialized at 20:1-20:5)
  in outer at 10:2-10:8
  in inner at 15:1-15:12";
        assert_eq!(display, expected);
    }

    #[test]
    fn test_eval_error_push_frame() {
        let span = test_span(1, 1, 1, 5);
        let mut err = EvalError::new("error", span);

        // Verify initial state has no frames
        assert!(err.stack.is_empty());

        // Push a frame directly
        let frame_span = test_span(5, 1, 5, 10);
        err.push_frame("first_function", frame_span.clone());

        // Verify frame was added
        assert_eq!(err.stack.len(), 1);
        assert_eq!(err.stack[0].label, "first_function");
        assert_eq!(err.stack[0].span, frame_span);

        // Push a second frame
        let frame2_span = test_span(10, 3, 10, 15);
        err.push_frame("second_function", frame2_span.clone());

        // Verify both frames are present
        assert_eq!(err.stack.len(), 2);
        assert_eq!(err.stack[1].label, "second_function");
        assert_eq!(err.stack[1].span, frame2_span);
    }
}
