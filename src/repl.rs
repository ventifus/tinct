//! REPL (Read-Eval-Print Loop) for the LLT language.
//!
//! The REPL mirrors `eval_document()` scope chain semantics: each input is parsed
//! and evaluated, and if the result is a Dict, its string-keyed entries become
//! bindings in a child environment for subsequent inputs. The previous result is
//! always accessible as `%`.
//!
//! ## Architecture
//!
//! The module is split into core logic and I/O:
//!
//! - **Core**: [`ReplSession`], [`bracket_count`], [`is_balanced`] -- pure logic
//!   with no terminal dependencies, fully testable.
//! - **I/O**: [`run_repl`] -- uses `rustyline` for line editing (behind the `repl`
//!   feature flag).

use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{create_stdlib_env_with_arena, MAX_FILE_SIZE};
use crate::eval::{deep_materialize, eval_file_with_input, materialize};
use crate::parser::parse;
use crate::typecheck::{DocMap, TypeMap};
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
/// Holds the current environment (scope chain) and the previous result (`%`).
/// Each successful evaluation may extend the environment (if the result is a Dict)
/// and always updates `%`.
pub struct ReplSession {
    /// Current lexical environment. Grows as Dict results add bindings.
    env: Arc<RwLock<Environment>>,
    /// The previous evaluation result, accessible as `%`.
    prev_result: Arc<Thunk>,
    /// Evaluation context for session (include guard, etc.)
    ctx: Arc<crate::eval::EvalContext>,
    /// Type information from the most recent type check.
    type_map: TypeMap,
    /// Documentation strings extracted from annotations.
    doc_map: DocMap,
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
        let (stdlib_env, stdlib_arena) =
            create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
        Self::with_env_and_arena(stdlib_env, stdlib_arena)
    }

    /// Create a new REPL session using a pre-created stdlib environment.
    ///
    /// This allows the caller to share the same `stdlib_env` with other
    /// infrastructure (e.g., `EvalContext`).
    ///
    /// **Note:** This method relies on `STDLIB_ARENA_CACHE` being populated by a prior
    /// call to `create_stdlib_env()` or `create_stdlib_env_with_arena()`. For explicit
    /// arena control, use `with_env_and_arena()` instead.
    pub fn with_env(stdlib_env: Arc<RwLock<Environment>>) -> Result<Self, String> {
        // Rely on STDLIB_ARENA_CACHE for arena snapshot
        let (_, stdlib_arena) = create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
        Self::with_env_and_arena(stdlib_env, stdlib_arena)
    }

    /// Create a new REPL session using a pre-created stdlib environment and arena.
    ///
    /// This is the explicit arena threading version that does NOT rely on
    /// `STDLIB_ARENA_CACHE`. Use this when you've already called
    /// `create_stdlib_env_with_arena()` and want to share the same arena.
    pub(crate) fn with_env_and_arena(
        stdlib_env: Arc<RwLock<Environment>>,
        stdlib_arena: Arc<std::sync::Mutex<crate::arena::ThunkArena>>,
    ) -> Result<Self, String> {
        // Create a session env as a child of stdlib, with % = empty dict.
        let session_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
            &stdlib_env,
        ))));
        let empty_dict = Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            Span::origin(),
        ));
        // Bind % as the pipeline variable (previous result), initially empty dict.
        session_env
            .write().unwrap()
            .insert("%".to_string(), Arc::clone(&empty_dict));

        // Create REPL session context (REPL runs in current directory, no sandbox)
        // AMBIENT-OK: REPL is an interactive session; operator has explicitly invoked it in CWD.
        #[allow(clippy::disallowed_methods)]
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .map_err(|e| format!("cannot open current directory: {e}"))?;
        let ctx = crate::eval::EvalContext::new_sharing_arena(
            base_dir,
            Arc::clone(&stdlib_env),
            false,
            stdlib_arena,
            std::collections::HashMap::new(), // REPL doesn't track macro injects yet
        );

        Ok(Self {
            env: session_env,
            prev_result: empty_dict,
            ctx,
            type_map: TypeMap::new(),
            doc_map: DocMap::new(),
        })
    }

    /// Evaluate a complete input string (one or more expressions forming a scope chain).
    ///
    /// Mirrors `eval_document()` semantics:
    /// 1. Parse the input as a full LLT source (file with one document).
    /// 2. Evaluate the document's expressions as a scope chain: intermediate Dict
    ///    results extend the environment, the last expression is the final result.
    /// 3. If the final result is a Dict, its string-keyed entries become bindings
    ///    in the session environment.
    /// 4. `%` is updated to the result thunk.
    ///
    /// On error, the session state is unchanged (the environment and `%` are not
    /// modified), so the user can fix and retry.
    pub fn eval_input(&mut self, input: &str) -> StepResult {
        if input.len() as u64 > MAX_FILE_SIZE {
            return Err(format!(
                "input exceeds the 10 MB limit ({} bytes)",
                MAX_FILE_SIZE
            ));
        }

        let parse_output = parse(input).map_err(|e| format!("{e}"))?;

        // Display all recovered parse errors (non-fatal errors inside bracket forms)
        if !parse_output.errors.is_empty() {
            for err in &parse_output.errors {
                eprintln!("parse error: {}", err);
            }
            // Continue evaluation despite parse errors — the AST contains Expr::Error nodes
        }

        let mut file = parse_output.file;

        // Desugar $_ implicit lambdas before evaluation
        crate::desugar::desugar_file(&mut file.node);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        crate::resolve::resolve_file(&file.node);
        // Type errors are advisory; evaluation proceeds regardless.
        // Collect type and doc information for meta-commands.
        let (_type_errors, type_map, doc_map, _scheme_map, _diagnostics) =
            crate::typecheck::typecheck_file_with_types(&file.node);
        // Extend (not replace) the session's type and doc maps with the new information
        self.type_map.extend(type_map);
        self.doc_map.extend(doc_map);

        if file.node.documents.is_empty() {
            return Err("empty input".to_string());
        }

        // Delegate to the same eval pipeline used by `llt eval`.
        let result_thunk = eval_file_with_input(
            &file.node,
            Arc::clone(&self.env),
            &self.ctx,
            Some(Arc::clone(&self.prev_result)),
        )
        .map_err(|e| {
            let mut error_str = format!("{e}");
            if let Some(snippet) = crate::render_span_snippet(input, e.definition_span) {
                error_str.push('\n');
                error_str.push_str(&snippet);
            }
            error_str
        })?;

        let val = materialize(&result_thunk, None, &self.ctx).map_err(|e| {
            let mut error_str = format!("{e}");
            if let Some(snippet) = crate::render_span_snippet(input, e.definition_span) {
                error_str.push('\n');
                error_str.push_str(&snippet);
            }
            error_str
        })?;
        let forced = deep_materialize(&val, &self.ctx, None).map_err(|e| {
            let mut error_str = format!("{e}");
            if let Some(snippet) = crate::render_span_snippet(input, e.definition_span) {
                error_str.push('\n');
                error_str.push_str(&snippet);
            }
            error_str
        })?;
        let display = value_to_display_string(&forced, &self.ctx).map_err(|e| {
            let mut error_str = format!("{e}");
            if let Some(snippet) = crate::render_span_snippet(input, e.definition_span) {
                error_str.push('\n');
                error_str.push_str(&snippet);
            }
            error_str
        })?;

        // Success: commit the result to session state.
        self.prev_result = result_thunk;

        // If the result is a Dict, extend the session env with its bindings.
        if let Value::Dict(ref map) = val {
            let child_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&self.env))));
            child_env
                .write().unwrap()
                .insert("%".to_string(), Arc::clone(&self.prev_result));
            for (key, val_thunk_id) in map {
                if let Key::String(name) = key {
                    let val_thunk = self.ctx.get_thunk(*val_thunk_id);
                    child_env.write().unwrap().insert(name.clone(), val_thunk);
                }
            }
            self.env = child_env;
        } else {
            self.env
                .write().unwrap()
                .insert("%".to_string(), Arc::clone(&self.prev_result));
        }

        Ok(display)
    }

    /// Handle REPL meta-commands (lines starting with `:`).
    ///
    /// Returns `true` to continue the REPL loop, `false` to exit.
    fn handle_meta_command(&self, line: &str) -> bool {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).map(|s| s.trim());

        match cmd {
            ":describe" => {
                if let Some(name) = arg {
                    self.describe(name);
                } else {
                    eprintln!("usage: :describe <name>");
                }
                true
            }
            ":type" => {
                if let Some(name) = arg {
                    self.show_type(name);
                } else {
                    eprintln!("usage: :type <name>");
                }
                true
            }
            ":help" => {
                self.show_help();
                true
            }
            _ => {
                eprintln!("Unknown command: {cmd}");
                eprintln!("Type :help for available commands.");
                true
            }
        }
    }

    /// Show type signature and documentation for a binding.
    fn describe(&self, name: &str) {
        let type_str = self.lookup_type(name);
        let doc_str = self.doc_map.get(name);

        match (type_str, doc_str) {
            (Some(ty), Some(doc)) => {
                println!("{name} : {ty}");
                println!();
                println!("{doc}");
            }
            (Some(ty), None) => {
                println!("{name} : {ty}");
            }
            (None, Some(doc)) => {
                println!("{name} : <unknown type>");
                println!();
                println!("{doc}");
            }
            (None, None) => {
                eprintln!("not found: {name}");
            }
        }
    }

    /// Show only the type signature for a binding.
    fn show_type(&self, name: &str) {
        if let Some(ty) = self.lookup_type(name) {
            println!("{name} : {ty}");
        } else {
            eprintln!("not found: {name}");
        }
    }

    /// Look up the type of a binding by searching the environment and TypeMap.
    ///
    /// Returns the type as a string if found. This searches the session's
    /// environment chain to find the binding, then looks up its type in the
    /// TypeMap if available.
    fn lookup_type(&self, name: &str) -> Option<String> {
        // Try to look up the binding in the environment to get its span
        let binding = self.env.read().unwrap().get(name)?;

        // The thunk has a span we can use to look up the type
        let span = binding.span;
        let key = (span.start.offset, span.end.offset);

        self.type_map.get(&key).map(|ty| format!("{ty}"))
    }

    /// Display available REPL meta-commands.
    fn show_help(&self) {
        println!("REPL meta-commands:");
        println!("  :describe <name>  Show type and documentation for a binding");
        println!("  :type <name>      Show type signature only");
        println!("  :help             Show this help message");
    }
}

// ── rustyline I/O (behind `repl` feature flag) ─────────────────────────────

/// Primary prompt shown when the REPL is ready for input.
const PROMPT_PRIMARY: &str = "tinct> ";
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
    let (stdlib_env, stdlib_arena) = create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
    let mut session = ReplSession::with_env_and_arena(Arc::clone(&stdlib_env), stdlib_arena)?;

    // The ReplSession already has an EvalContext set up with CWD as base_dir,
    // so $include will work correctly using the context threading.
    (|| -> Result<(), String> {
        let mut editor =
            DefaultEditor::new().map_err(|e| format!("failed to initialize editor: {e}"))?;

        // Try to load history from ~/.tinct_history (best-effort).
        let history_path =
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".tinct_history"));
        if let Some(ref path) = history_path {
            let _ = editor.load_history(path);
        }

        eprintln!("tinct REPL (Ctrl-D to exit)");

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

                    // Check for meta-commands (lines starting with ':')
                    if buffer.trim_start().starts_with(':') {
                        session.handle_meta_command(buffer.trim());
                        buffer.clear();
                        bracket_depth = 0;
                        continue;
                    }

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
    })()
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
        assert!(is_balanced(&["[fn [x]", "  [+ x 1]]"]));
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
        assert_eq!(session.eval_input("x").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_dict_overwrites_previous() {
        let mut session = ReplSession::new().unwrap();

        // First dict sets x.
        session.eval_input("[x: 1]").unwrap();

        // Second dict overwrites x.
        session.eval_input("[x: 99]").unwrap();

        // x should be the new value.
        assert_eq!(session.eval_input("x").unwrap(), "Int(99)");
    }

    #[test]
    fn test_session_percent_pipeline() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a value.
        session.eval_input("42").unwrap();

        // % should be the previous result.
        assert_eq!(session.eval_input("%").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_percent_dict_access() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a dict.
        session.eval_input("[name: \"Alice\" age: 30]").unwrap();

        // Access a field through %.
        assert_eq!(session.eval_input("%.name").unwrap(), "String(\"Alice\")");
    }

    #[test]
    fn test_session_percent_initial_empty_dict() {
        let mut session = ReplSession::new().unwrap();

        // % should initially be an empty dict.
        assert_eq!(session.eval_input("%").unwrap(), "Dict({})");
    }

    #[test]
    fn test_session_error_recovery() {
        let mut session = ReplSession::new().unwrap();

        // First: set a value.
        session.eval_input("[x: 42]").unwrap();

        // Second: cause an error (undefined variable).
        assert!(session.eval_input("nonexistent").is_err());

        // Third: session should still work; previous bindings intact.
        assert_eq!(session.eval_input("x").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_error_does_not_update_percent() {
        let mut session = ReplSession::new().unwrap();

        // Set % to 42.
        session.eval_input("42").unwrap();

        // Cause an error.
        assert!(session.eval_input("nonexistent").is_err());

        // % should still be 42 (error did not update it).
        assert_eq!(session.eval_input("%").unwrap(), "Int(42)");
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
        assert_eq!(session.eval_input("[x: 10]\n[+ x 5]").unwrap(), "Int(15)");
    }

    #[test]
    fn test_session_stdlib_available() {
        let mut session = ReplSession::new().unwrap();

        // Builtins should be accessible.
        assert_eq!(session.eval_input("[+ 1 2]").unwrap(), "Int(3)");
    }

    #[test]
    fn test_session_stdlib_string_builtins() {
        let mut session = ReplSession::new().unwrap();

        // str-to-upper-char is a private Rust builtin accessible only via [include %rust "string"]
        // inside the prelude/strings modules — it is NOT directly available in user-facing REPL scope.
        // Test a prelude-exported string function instead: join (wrapper over builtin-join).
        assert_eq!(
            session
                .eval_input("[join \", \" [\"a\" \"b\" \"c\"]]")
                .unwrap(),
            "String(\"a, b, c\")"
        );
    }

    #[test]
    fn test_session_function_definition_and_call() {
        let mut session = ReplSession::new().unwrap();

        // Define a function (use [let ...] param form, not bare-param form).
        session
            .eval_input("[double: [fn [let x] [* x 2]]]")
            .unwrap();

        // Call the function.
        assert_eq!(session.eval_input("[double 21]").unwrap(), "Int(42)");
    }

    #[test]
    fn test_session_non_dict_does_not_extend_env() {
        let mut session = ReplSession::new().unwrap();

        // Evaluate a scalar; it shouldn't add any bindings.
        session.eval_input("42").unwrap();

        // There should be no new bindings (only % and builtins).
        // Trying to access a non-existent var should still fail.
        assert!(session.eval_input("x").is_err());
    }

    #[test]
    fn test_session_cumulative_dict_bindings() {
        let mut session = ReplSession::new().unwrap();

        // First dict.
        session.eval_input("[x: 1]").unwrap();

        // Second dict with different key.
        session.eval_input("[y: 2]").unwrap();

        // Both bindings should be accessible (y from current env, x from parent).
        assert_eq!(session.eval_input("[+ x y]").unwrap(), "Int(3)");
    }

    #[test]
    fn test_session_percent_updates_after_each_eval() {
        let mut session = ReplSession::new().unwrap();

        // First eval.
        session.eval_input("1").unwrap();

        assert_eq!(session.eval_input("%").unwrap(), "Int(1)");

        // % itself becomes the new %, so % should still be 1 (now it was
        // just the result of evaluating %, which was 1).
        assert_eq!(session.eval_input("[+ % 10]").unwrap(), "Int(11)");

        // Now % is 11.
        assert_eq!(session.eval_input("%").unwrap(), "Int(11)");
    }

    #[test]
    fn test_session_nested_dict() {
        let mut session = ReplSession::new().unwrap();

        // MAX_DISPLAY_DEPTH=5, so 4-level nesting is fully displayed
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
        let err = session.eval_input("42\n[+ 1 2]").unwrap_err();
        assert!(
            err.contains("expected"),
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

        assert_eq!(session.eval_input("[* [+ 2 3] 4]").unwrap(), "Int(20)");
    }

    #[test]
    fn test_session_if_builtin() {
        let mut session = ReplSession::new().unwrap();

        assert_eq!(session.eval_input("[if true 1 0]").unwrap(), "Int(1)");

        assert_eq!(session.eval_input("[if false 1 0]").unwrap(), "Int(0)");
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

        // Multi-document input: first document produces [x: 1], second accesses %.x.
        // Documents are separated by `---`.
        let display = session.eval_input("[x: 1]\n---\n%.x").unwrap();
        assert_eq!(display, "Int(1)");
    }

    #[test]
    fn test_session_cycle_detection() {
        let mut session = ReplSession::new().unwrap();

        // Create a dict with a circular dependency: x references y, y references x.
        // deep_materialize will detect the cycle when forcing all values.
        let msg = session.eval_input("[x: y  y: x]").unwrap_err();
        assert!(
            msg.contains("circular"),
            "expected circular dependency error, got: {msg}"
        );
    }

    #[test]
    fn test_session_depth_exhaustion() {
        // 256 levels of LLT recursion needs more than the default 8MB Rust stack.
        let result = std::thread::Builder::new()
            .stack_size(128 * 1024 * 1024) // 128MB — debug-mode materialize() needs ~100MB at 256 levels
            .spawn(|| {
                let mut session = ReplSession::new().unwrap();
                session.eval_input("[f: [fn [x] [f [+ x 1]]]]").unwrap();
                session.eval_input("[f 0]").unwrap_err()
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

    // ── Integration tests: REPL session behavior ────────────────────────────

    /// Multi-line input spanning multiple expressions: the REPL should accumulate
    /// lines until is_balanced() returns true, then evaluate the joined buffer.
    /// This test verifies that a function body defined across multiple lines
    /// evaluates correctly when submitted as a single (joined) input string.
    #[test]
    fn test_session_multiline_function_body() {
        let mut session = ReplSession::new().unwrap();

        // Simulate the REPL's buffer-join behavior: lines are joined with '\n'
        // and submitted together once the brackets are balanced.
        // Use [let ...] param form (bare param form is no longer supported).
        let multiline_input = "[add:\n  [fn [let x y]\n    [+ x y]]]";
        session.eval_input(multiline_input).unwrap();

        let result = session.eval_input("[add 10 32]").unwrap();
        assert_eq!(result, "Int(42)");
    }

    /// Syntax errors in the REPL do not kill the session: the environment is
    /// unchanged after a parse error, and subsequent successful inputs work.
    #[test]
    fn test_session_syntax_error_does_not_kill_session() {
        let mut session = ReplSession::new().unwrap();

        // Establish a binding.
        session.eval_input("[x: 100]").unwrap();

        // Submit a syntax error (unclosed bracket — parse returns Err for unbalanced input).
        // The session should return Err, but state must be preserved.
        let err = session.eval_input("[broken syntax !!!@#$");
        // May or may not be a parse error depending on recovery — just ensure session survives.
        drop(err);

        // The session is still alive: previous binding is accessible.
        assert_eq!(session.eval_input("x").unwrap(), "Int(100)");
    }

    /// Function definition in one eval_input followed by a call in a separate
    /// eval_input: tests that the definition persists across session steps and
    /// that named/positional arguments work.
    #[test]
    fn test_session_function_def_then_call_separate_steps() {
        let mut session = ReplSession::new().unwrap();

        // Step 1: define a function (use [let ...] param form).
        session
            .eval_input("[square: [fn [let n] [* n n]]]")
            .unwrap();

        // Step 2: call the function in a separate eval.
        let result = session.eval_input("[square 9]").unwrap();
        assert_eq!(result, "Int(81)");
    }

    // ── Meta-command tests ──────────────────────────────────────────────────

    #[test]
    fn test_meta_command_handle_help() {
        let session = ReplSession::new().unwrap();

        // :help should return true (continue REPL)
        assert!(session.handle_meta_command(":help"));
    }

    #[test]
    fn test_meta_command_handle_unknown() {
        let session = ReplSession::new().unwrap();

        // Unknown commands should return true (continue REPL) but print error
        assert!(session.handle_meta_command(":unknown"));
    }

    #[test]
    fn test_meta_command_type_without_arg() {
        let session = ReplSession::new().unwrap();

        // :type without argument should return true (prints usage)
        assert!(session.handle_meta_command(":type"));
    }

    #[test]
    fn test_meta_command_describe_without_arg() {
        let session = ReplSession::new().unwrap();

        // :describe without argument should return true (prints usage)
        assert!(session.handle_meta_command(":describe"));
    }
}
