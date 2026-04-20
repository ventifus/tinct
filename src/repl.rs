//! REPL (Read-Eval-Print Loop) for the LLT language.
//!
//! The REPL mirrors `eval_document()` scope chain semantics: each input is parsed
//! and evaluated, and if the result is a Dict, its string-keyed entries become
//! bindings in a child environment for subsequent inputs. The previous result is
//! always accessible as `$$`.
//!
//! ## Architecture
//!
//! The module is split into core logic and I/O:
//!
//! - **Core**: [`ReplSession`], [`bracket_count`], [`is_balanced`] -- pure logic
//!   with no terminal dependencies, fully testable.
//! - **I/O**: [`run_repl`] -- uses `rustyline` for line editing (behind the `repl`
//!   feature flag).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{
    clear_include_context, create_stdlib_env, set_include_context, IncludeContext, MAX_FILE_SIZE,
};
use crate::eval::{deep_materialize, eval_file_with_input, materialize};
use crate::parser::parse;
use crate::value::{Environment, Key, Thunk, Value};
use crate::value_to_display_string;

// ── Core types and logic (no rustyline dependency) ──────────────────────────

/// Count the number of unmatched opening brackets in a string.
///
/// Returns a positive number when there are more `[` than `]`, zero when balanced,
/// and a negative number when there are excess `]`. String literals (double-quoted)
/// are skipped so that brackets inside strings don't affect the count.
pub fn bracket_count(input: &str) -> i32 {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;

    for ch in input.chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '[' => depth += 1,
            ']' => depth -= 1,
            '#' => break, // rest of line is a comment
            _ => {}
        }
    }
    depth
}

/// Returns `true` when the accumulated bracket depth across all lines is <= 0
/// (i.e., all opened brackets have been closed or the input is a simple expression).
pub fn is_balanced(lines: &[&str]) -> bool {
    let total: i32 = lines.iter().map(|l| bracket_count(l)).sum();
    total <= 0
}

/// Persistent REPL session state.
///
/// Holds the current environment (scope chain) and the previous result (`$$`).
/// Each successful evaluation may extend the environment (if the result is a Dict)
/// and always updates `$$`.
pub struct ReplSession {
    /// Current lexical environment. Grows as Dict results add bindings.
    env: Rc<RefCell<Environment>>,
    /// The previous evaluation result, accessible as `$$`.
    prev_result: Rc<Thunk>,
}

/// The outcome of a single REPL evaluation step.
///
/// `Ok(display)` on success (the display string for the result),
/// `Err(message)` on parse or evaluation error.
pub type StepResult = Result<String, String>;

impl ReplSession {
    /// Create a new REPL session with the standard library environment.
    ///
    /// Returns an error if the stdlib fails to load (e.g., prelude parse error).
    pub fn new() -> Result<Self, String> {
        let stdlib_env = create_stdlib_env().map_err(|e| format!("{e}"))?;
        Ok(Self::with_env(stdlib_env))
    }

    /// Create a new REPL session using a pre-created stdlib environment.
    ///
    /// This allows the caller to share the same `stdlib_env` with other
    /// infrastructure (e.g., `IncludeContext`).
    pub fn with_env(stdlib_env: Rc<RefCell<Environment>>) -> Self {
        // Create a session env as a child of stdlib, with $$ = empty dict.
        let session_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
            &stdlib_env,
        ))));
        let empty_dict = Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            Span::origin(),
        ));
        // The parser strips the leading `$` sigil, so `$$` becomes VarRef("$").
        session_env
            .borrow_mut()
            .insert("$".to_string(), Rc::clone(&empty_dict));

        Self {
            env: session_env,
            prev_result: empty_dict,
        }
    }

    /// Evaluate a complete input string (one or more expressions forming a scope chain).
    ///
    /// Mirrors `eval_document()` semantics:
    /// 1. Parse the input as a full LLT source (file with one document).
    /// 2. Evaluate the document's expressions as a scope chain: intermediate Dict
    ///    results extend the environment, the last expression is the final result.
    /// 3. If the final result is a Dict, its string-keyed entries become bindings
    ///    in the session environment.
    /// 4. `$$` is updated to the result thunk.
    ///
    /// On error, the session state is unchanged (the environment and `$$` are not
    /// modified), so the user can fix and retry.
    pub fn eval_input(&mut self, input: &str) -> StepResult {
        if input.len() as u64 > MAX_FILE_SIZE {
            return Err(format!(
                "input exceeds the 10 MB limit ({} bytes)",
                MAX_FILE_SIZE
            ));
        }

        let file = parse(input).map_err(|e| format!("{e}"))?;

        if file.node.documents.is_empty() {
            return Err("empty input".to_string());
        }

        // Delegate to the same eval pipeline used by `llt eval`.
        let result_thunk = eval_file_with_input(
            &file.node,
            Rc::clone(&self.env),
            Some(Rc::clone(&self.prev_result)),
            0,
        )
        .map_err(|e| format!("{e}"))?;

        let val = materialize(&result_thunk, None, 0).map_err(|e| format!("{e}"))?;
        let forced = deep_materialize(&val, 0).map_err(|e| format!("{e}"))?;
        let display = value_to_display_string(&forced, 0).map_err(|e| format!("{e}"))?;

        // Success: commit the result to session state.
        self.prev_result = result_thunk;

        // If the result is a Dict, extend the session env with its bindings.
        if let Value::Dict(ref map) = val {
            let child_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&self.env))));
            child_env
                .borrow_mut()
                .insert("$".to_string(), Rc::clone(&self.prev_result));
            for (key, val_thunk) in map {
                if let Key::String(name) = key {
                    child_env
                        .borrow_mut()
                        .insert(name.clone(), Rc::clone(val_thunk));
                }
            }
            self.env = child_env;
        } else {
            self.env
                .borrow_mut()
                .insert("$".to_string(), Rc::clone(&self.prev_result));
        }

        Ok(display)
    }
}

// ── rustyline I/O (behind `repl` feature flag) ─────────────────────────────

/// Primary prompt shown when the REPL is ready for input.
const PROMPT_PRIMARY: &str = "llt> ";
/// Continuation prompt shown during multi-line input (unbalanced brackets).
const PROMPT_CONTINUATION: &str = "...> ";

/// Run the interactive REPL.
///
/// Uses `rustyline` for line editing and history. Exits cleanly on Ctrl-D (EOF).
/// Ctrl-C cancels the current multi-line input if any, otherwise prints a hint.
///
/// Requires the `repl` feature flag (which enables the `rustyline` dependency).
#[cfg(feature = "repl")]
pub fn run_repl() -> Result<(), String> {
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

    // Create the stdlib env once and share it between the session and $include context.
    let stdlib_env = create_stdlib_env().map_err(|e| format!("{e}"))?;
    let mut session = ReplSession::with_env(Rc::clone(&stdlib_env));

    // Set up $include context so that $include works in the REPL.
    // Use CWD as the base directory for relative path resolution.
    let base_dir =
        std::env::current_dir().map_err(|e| format!("cannot determine working directory: {e}"))?;
    set_include_context(IncludeContext {
        base_dir,
        include_guard: Rc::new(RefCell::new(HashSet::new())),
        stdlib_env,
    });

    // Wrap the main loop so that clear_include_context() runs on all exit paths.
    let result = (|| -> Result<(), String> {
        let mut editor =
            DefaultEditor::new().map_err(|e| format!("failed to initialize editor: {e}"))?;

        // Try to load history from ~/.llt_history (best-effort).
        let history_path =
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".llt_history"));
        if let Some(ref path) = history_path {
            let _ = editor.load_history(path);
        }

        eprintln!("Lazy Lisp Transformer REPL (Ctrl-D to exit)");

        let mut buffer = String::new();
        let mut bracket_depth: i32 = 0;

        loop {
            let prompt = if bracket_depth > 0 {
                PROMPT_CONTINUATION
            } else {
                PROMPT_PRIMARY
            };

            match editor.readline(prompt) {
                Ok(line) => {
                    // Check buffer size before appending to prevent unbounded growth.
                    if buffer.len() + line.len() > MAX_FILE_SIZE as usize {
                        eprintln!(
                            "error: input exceeds the 10 MB limit ({} bytes)",
                            MAX_FILE_SIZE
                        );
                        buffer.clear();
                        bracket_depth = 0;
                        continue;
                    }

                    bracket_depth += bracket_count(&line);

                    if buffer.is_empty() {
                        buffer = line;
                    } else {
                        buffer.push('\n');
                        buffer.push_str(&line);
                    }

                    // Wait for brackets to balance before evaluating.
                    if bracket_depth > 0 {
                        continue;
                    }

                    // Skip empty/whitespace-only input.
                    if buffer.trim().is_empty() {
                        buffer.clear();
                        bracket_depth = 0;
                        continue;
                    }

                    let _ = editor.add_history_entry(buffer.as_str());

                    match session.eval_input(&buffer) {
                        Ok(display) => {
                            println!("{display}");
                        }
                        Err(msg) => {
                            eprintln!("error: {msg}");
                        }
                    }

                    buffer.clear();
                    bracket_depth = 0;
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl-C: cancel current multi-line input, or print hint.
                    if !buffer.is_empty() {
                        eprintln!("^C (input cancelled)");
                        buffer.clear();
                        bracket_depth = 0;
                    } else {
                        eprintln!("(use Ctrl-D to exit)");
                    }
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl-D: exit cleanly.
                    eprintln!("Goodbye.");
                    break;
                }
                Err(e) => {
                    eprintln!("readline error: {e}");
                    break;
                }
            }
        }

        // Save history (best-effort).
        if let Some(ref path) = history_path {
            let _ = editor.save_history(path);
        }

        Ok(())
    })(); // end of closure wrapping the main loop

    clear_include_context();
    result
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── bracket_count tests ─────────────────────────────────────────────

    #[test]
    fn test_bracket_count_empty() {
        assert_eq!(bracket_count(""), 0);
    }

    #[test]
    fn test_bracket_count_no_brackets() {
        assert_eq!(bracket_count("hello world"), 0);
    }

    #[test]
    fn test_bracket_count_balanced() {
        assert_eq!(bracket_count("[x: 1]"), 0);
    }

    #[test]
    fn test_bracket_count_open() {
        assert_eq!(bracket_count("[x: 1"), 1);
    }

    #[test]
    fn test_bracket_count_close() {
        assert_eq!(bracket_count("x: 1]"), -1);
    }

    #[test]
    fn test_bracket_count_nested() {
        assert_eq!(bracket_count("[["), 2);
    }

    #[test]
    fn test_bracket_count_nested_balanced() {
        assert_eq!(bracket_count("[[x: 1] [y: 2]]"), 0);
    }

    #[test]
    fn test_bracket_count_string_literal_ignored() {
        // Brackets inside string literals should not be counted.
        assert_eq!(bracket_count("\"[not a bracket]\""), 0);
    }

    #[test]
    fn test_bracket_count_escaped_quote_in_string() {
        // An escaped quote inside a string shouldn't close the string.
        assert_eq!(bracket_count("\"hello \\\" world\""), 0);
    }

    #[test]
    fn test_bracket_count_comment_ignored() {
        // Everything after # is a comment; brackets in comments don't count.
        assert_eq!(bracket_count("[x: 1 # [unclosed"), 1);
    }

    #[test]
    fn test_bracket_count_comment_only() {
        assert_eq!(bracket_count("# [[["), 0);
    }

    #[test]
    fn test_bracket_count_mixed() {
        assert_eq!(bracket_count("[fn [x]"), 1);
    }

    #[test]
    fn test_bracket_count_deeply_nested() {
        assert_eq!(bracket_count("[[["), 3);
        assert_eq!(bracket_count("]]]"), -3);
    }

    // ── is_balanced tests ───────────────────────────────────────────────

    #[test]
    fn test_is_balanced_single_line() {
        assert!(is_balanced(&["[x: 1]"]));
    }

    #[test]
    fn test_is_balanced_multi_line_balanced() {
        assert!(is_balanced(&["[x:", "  1]"]));
    }

    #[test]
    fn test_is_balanced_multi_line_unbalanced() {
        assert!(!is_balanced(&["[x:", "  [y:"]));
    }

    #[test]
    fn test_is_balanced_empty() {
        assert!(is_balanced(&[""]));
    }

    #[test]
    fn test_is_balanced_no_brackets() {
        assert!(is_balanced(&["42"]));
    }

    #[test]
    fn test_is_balanced_excess_close() {
        // Excess closing brackets are "balanced" (depth <= 0).
        assert!(is_balanced(&["[x: 1]]"]));
    }

    #[test]
    fn test_is_balanced_three_lines() {
        assert!(is_balanced(&["[", "  x: 1", "]"]));
    }

    #[test]
    fn test_is_balanced_nested_multi_line() {
        assert!(is_balanced(&["[fn [x]", "  [+ $x 1]]"]));
    }

    // ── ReplSession tests ───────────────────────────────────────────────

    #[test]
    fn test_session_simple_int() {
        let mut session = ReplSession::new().unwrap();
        assert_eq!(session.eval_input("42").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_simple_string() {
        let mut session = ReplSession::new().unwrap();
        assert_eq!(
            session.eval_input("\"hello\"").unwrap(),
            "String(\"hello\")"
        );
    }

    #[test]
    fn test_session_simple_bool() {
        let mut session = ReplSession::new().unwrap();
        assert_eq!(session.eval_input("true").unwrap(), "Bool(true)");
    }

    #[test]
    fn test_session_simple_dict() {
        let mut session = ReplSession::new().unwrap();
        assert_eq!(
            session.eval_input("[x: 1 y: 2]").unwrap(),
            "Dict({\"x\": Int(1), \"y\": Int(2)})"
        );
    }

    #[test]
    fn test_session_dict_extends_env() {
        let mut session = ReplSession::new().unwrap();

        // First input: a dict that should extend the environment.
        session.eval_input("[x: 42]").unwrap();

        // Second input: reference the binding from the previous dict.
        assert_eq!(session.eval_input("$x").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_dict_overwrites_previous() {
        let mut session = ReplSession::new().unwrap();

        // First dict sets x.
        session.eval_input("[x: 1]").unwrap();

        // Second dict overwrites x.
        session.eval_input("[x: 99]").unwrap();

        // x should be the new value.
        assert_eq!(session.eval_input("$x").unwrap(), "Int(99)");
    }

    #[test]
    fn test_session_dollar_dollar_pipeline() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a value.
        session.eval_input("42").unwrap();

        // $$ should be the previous result.
        assert_eq!(session.eval_input("$$").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_dollar_dollar_dict_access() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a dict.
        session.eval_input("[name: Alice age: 30]").unwrap();

        // Access a field through $$.
        assert_eq!(session.eval_input("$$.name").unwrap(), "String(\"Alice\")");
    }

    #[test]
    fn test_session_dollar_dollar_initial_empty_dict() {
        let mut session = ReplSession::new().unwrap();

        // $$ should initially be an empty dict.
        assert_eq!(session.eval_input("$$").unwrap(), "Dict({})");
    }

    #[test]
    fn test_session_error_recovery() {
        let mut session = ReplSession::new().unwrap();

        // First: set a value.
        session.eval_input("[x: 42]").unwrap();

        // Second: cause an error (undefined variable).
        assert!(session.eval_input("$nonexistent").is_err());

        // Third: session should still work; previous bindings intact.
        assert_eq!(session.eval_input("$x").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_error_does_not_update_dollar_dollar() {
        let mut session = ReplSession::new().unwrap();

        // Set $$ to 42.
        session.eval_input("42").unwrap();

        // Cause an error.
        assert!(session.eval_input("$nonexistent").is_err());

        // $$ should still be 42 (error did not update it).
        assert_eq!(session.eval_input("$$").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_parse_error() {
        let mut session = ReplSession::new().unwrap();
        assert!(session.eval_input("[unterminated").is_err());
    }

    #[test]
    fn test_session_scope_chain_in_single_input() {
        let mut session = ReplSession::new().unwrap();

        // Multiple expressions in a single input form a scope chain.
        // First expression is a Dict (creates bindings), second uses them.
        assert_eq!(
            session.eval_input("[x: 10]\n[call $+ $x 5]").unwrap(),
            "Int(15)"
        );
    }

    #[test]
    fn test_session_stdlib_available() {
        let mut session = ReplSession::new().unwrap();

        // Builtins should be accessible.
        assert_eq!(session.eval_input("[call $+ 1 2]").unwrap(), "Int(3)");
    }

    #[test]
    fn test_session_stdlib_string_builtins() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(
            session.eval_input("[call $upper \"hello\"]").unwrap(),
            "String(\"HELLO\")"
        );
    }

    #[test]
    fn test_session_function_definition_and_call() {
        let mut session = ReplSession::new().unwrap();

        // Define a function.
        session
            .eval_input("[double: [fn [x] [call $* $x 2]]]")
            .unwrap();

        // Call the function.
        assert_eq!(session.eval_input("[call $double 21]").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_non_dict_does_not_extend_env() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a scalar; it shouldn't add any bindings.
        session.eval_input("42").unwrap();

        // There should be no new bindings (only $$ and builtins).
        // Trying to access a non-existent var should still fail.
        assert!(session.eval_input("$x").is_err());
    }

    #[test]
    fn test_session_cumulative_dict_bindings() {
        let mut session = ReplSession::new().unwrap();

        // First dict.
        session.eval_input("[x: 1]").unwrap();

        // Second dict with different key.
        session.eval_input("[y: 2]").unwrap();

        // Both bindings should be accessible (y from current env, x from parent).
        assert_eq!(session.eval_input("[call $+ $x $y]").unwrap(), "Int(3)");
    }

    #[test]
    fn test_session_dollar_dollar_updates_after_each_eval() {
        let mut session = ReplSession::new().unwrap();

        // First eval.
        session.eval_input("1").unwrap();

        assert_eq!(session.eval_input("$$").unwrap(), "Int(1)");

        // $$ itself becomes the new $$, so $$ should still be 1 (now it was
        // just the result of evaluating $$, which was 1).
        assert_eq!(session.eval_input("[call $+ $$ 10]").unwrap(), "Int(11)");

        // Now $$ is 11.
        assert_eq!(session.eval_input("$$").unwrap(), "Int(11)");
    }

    #[test]
    fn test_session_nested_dict() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(
            session.eval_input("[a: [b: [c: 42]]]").unwrap(),
            "Dict({\"a\": Dict({\"b\": Dict({\"c\": Int(42)})})})"
        );
    }

    #[test]
    fn test_session_array_like_dict() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(
            session.eval_input("[10 20 30]").unwrap(),
            "Dict({0: Int(10), 1: Int(20), 2: Int(30)})"
        );
    }

    #[test]
    fn test_session_intermediate_non_dict_error() {
        let mut session = ReplSession::new().unwrap();

        // Two expressions where the first is not a Dict.
        let err = session.eval_input("42\n[call $+ 1 2]").unwrap_err();
        assert!(
            err.contains("type mismatch"),
            "expected type mismatch error, got: {err}"
        );
    }

    #[test]
    fn test_session_float() {
        let mut session = ReplSession::new().unwrap();
        assert_eq!(session.eval_input("3.14").unwrap(), "Float(3.14)");
    }

    #[test]
    fn test_session_arithmetic_chain() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(
            session.eval_input("[call $* [call $+ 2 3] 4]").unwrap(),
            "Int(20)"
        );
    }

    #[test]
    fn test_session_if_builtin() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(session.eval_input("[call $if true 1 0]").unwrap(), "Int(1)");

        assert_eq!(
            session.eval_input("[call $if false 1 0]").unwrap(),
            "Int(0)"
        );
    }

    #[test]
    fn test_session_input_size_limit() {
        let mut session = ReplSession::new().unwrap();
        // Create an input that exceeds MAX_FILE_SIZE (10 MB).
        let oversized = "x".repeat(MAX_FILE_SIZE as usize + 1);
        let err = session.eval_input(&oversized).unwrap_err();
        assert!(
            err.contains("10 MB limit"),
            "expected size limit error, got: {err}"
        );
    }

    // ── Multi-document, cycle, depth, whitespace, bracket_count edge cases ──

    #[test]
    fn test_session_multi_document_pipeline() {
        let mut session = ReplSession::new().unwrap();

        // Multi-document input: first document produces [x: 1], second accesses $$.x.
        // Documents are separated by `---`.
        let display = session.eval_input("[x: 1]\n---\n$$.x").unwrap();
        assert_eq!(display, "Int(1)");
    }

    #[test]
    fn test_session_cycle_detection() {
        let mut session = ReplSession::new().unwrap();

        // Create a dict with a circular dependency: x references y, y references x.
        // deep_materialize will detect the cycle when forcing all values.
        let msg = session.eval_input("[x: $y  y: $x]").unwrap_err();
        assert!(
            msg.contains("circular"),
            "expected circular dependency error, got: {msg}"
        );
    }

    #[test]
    fn test_session_depth_exhaustion() {
        // 256 levels of LLT recursion needs more than the default 8MB Rust stack.
        let result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut session = ReplSession::new().unwrap();
                session
                    .eval_input("[f: [fn [x] [call $f [call $+ $x 1]]]]")
                    .unwrap();
                session.eval_input("[call $f 0]").unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(
            result.contains("depth"),
            "expected depth exhaustion error, got: {result}"
        );
    }

    #[test]
    fn test_session_whitespace_only_input() {
        let mut session = ReplSession::new().unwrap();

        // Whitespace-only input: parser produces a document with 0 expressions,
        // which eval_document returns as an empty Dict (graceful, not an error).
        assert_eq!(session.eval_input("   ").unwrap(), "Dict({})");
        assert_eq!(session.eval_input("\n\n").unwrap(), "Dict({})");
    }

    #[test]
    fn test_bracket_count_unclosed_string() {
        // An unterminated string literal: the `[` inside should be treated as
        // part of the string (in_string stays true until EOF), so it should not
        // contribute to the bracket count.
        assert_eq!(bracket_count("\"[unclosed"), 0);
    }
}
