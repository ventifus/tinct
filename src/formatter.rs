// Source code pretty-printer. This module reformats LLT source for human reading.
//
// `tinct fmt` calls `format_source_tinct_with_dir`, which evaluates a tinct-hosted
// formatter script (`stdlib/cli/fmt/pretty.llt`) with the parsed AST dict as `%`.
// The Rust `Formatter` struct was deleted — it was only ever used by the now-removed
// `format_source` function that was dead code from `tinct fmt`'s perspective.
/// Format source using the tinct-hosted formatter script, optionally receiving an already-open
/// base directory to avoid re-acquiring ambient filesystem authority.
///
/// `base_dir` should be passed by callers (e.g., `src/main.rs`) that already hold an open Dir.
/// When `None`, falls back to opening the current working directory ambiently — this path is
/// used by the LSP server which does not have an open CWD Dir at the formatter call site.
pub async fn format_source_tinct_with_dir(
    input: &str,
    script_path: &std::path::Path,
    base_dir: Option<cap_std::fs::Dir>,
) -> Result<String, String> {
    use crate::desugar;
    use crate::eval::{self, EvalContext};
    use crate::parser::parse;
    use crate::resolve;
    use crate::typecheck;
    use crate::value::Value;
    use std::sync::Arc;

    // Determine mode from script name: compact.llt → minimal AST; everything else → full AST.
    let compact = script_path.file_stem().and_then(|s| s.to_str()) == Some("compact");

    // Open the base directory first (needed by EvalContext).
    let base_dir = match base_dir {
        Some(dir) => dir,
        None => {
            // AMBIENT-OK: fallback for callers (LSP) that do not hold an open CWD Dir.
            let base_dir_path = std::env::current_dir()
                .ok()
                .and_then(|d| d.canonicalize().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            #[allow(clippy::disallowed_methods)]
            cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
                .map_err(|e| format!("cannot open base directory: {e}"))?
        }
    };

    // Parse the input source (no env/ctx needed yet).
    let file: Arc<str> = Arc::from("<formatter>");
    let parse_output = parse(input, file).map_err(|e| format!("{e}"))?;

    // Load the formatter script BEFORE creating env/ctx.
    // AMBIENT-OK: formatter script loaded from stdlib path.
    #[allow(clippy::disallowed_methods)]
    let formatter_source = std::fs::read_to_string(script_path).map_err(|e| {
        format!(
            "cannot read formatter script {}: {e}",
            script_path.display()
        )
    })?;
    let formatter_file: Arc<str> = Arc::from(script_path.display().to_string().as_str());
    let formatter_parsed = parse(&formatter_source, formatter_file)
        .map_err(|e| format!("formatter parse error: {e}"))?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let mut formatter_program = formatter_parsed.program;
    desugar::desugar_program_full(&mut formatter_program);

    // Build a fresh core env BEFORE resolving so the resolver can be seeded from it.
    // This ensures builtin names (builtin-str, etc.) in the formatter script
    // resolve to de Bruijn coordinates instead of falling back to name-based lookup.
    let env = crate::builtins::build_core_env();

    // Variable resolution pass — builds ResolutionTable (NodeId → de Bruijn coordinates).
    // T-1576: formatter runs in bootstrap mode (no arena yet). The resolver uses empty
    // scope stack; builtins and % become FreeVars, falling back to name-based lookup.
    // This is acceptable for the formatter script — it doesn't need slot-based resolution.
    let (_table, _frames) = resolve::resolve_surface_program(&formatter_program, &[]);
    // Typecheck the desugared formatter (writes inline type annotations).
    let _ = typecheck::typecheck_surface_program_annotation_table(&formatter_program).await;

    let _ = env; // env no longer needed by EvalContext
    let ctx = EvalContext::new_empty(base_dir, false);

    // Convert input AST to dict using the now-stable ctx.
    use crate::surface_convert::{surface_program_to_dict, AstToDictOpts, CommentMaps};
    let opts = if compact {
        AstToDictOpts::default()
    } else {
        AstToDictOpts {
            source: Some(input),
            comments: Some(CommentMaps {
                leading_comments: &parse_output.leading_comments,
                trailing_comments: &parse_output.trailing_comments,
                blank_before: &parse_output.blank_before,
            }),
        }
    };
    let ast_thunk =
        surface_program_to_dict(&parse_output.program, &opts, &ctx).map_err(|e| format!("{e}"))?;

    // Evaluate formatter with AST as % (pipeline input).
    let formatter_thunk =
        eval::eval_surface_file_with_input(&formatter_program, &ctx, Some(ast_thunk))
            .await
            .map_err(|e| format!("formatter eval error: {e}"))?;

    // Materialize the result — the formatter script must return a bare String.
    // If it raises (calls `raise`), the error propagates as an EvalError here.
    let result_val = eval::materialize(&formatter_thunk, None, &ctx)
        .await
        .map_err(|e| format!("formatter error: {e}"))?;

    // Protocol: the formatter script returns a bare String on success.
    // Any other value is a protocol violation — the script should have returned
    // the string directly. If it needs to signal failure it should call `raise`.
    match result_val {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(source[start..end].to_string()),

        _ => {
            let display_str =
                crate::value_to_display_string(&result_val, &ctx, formatter_thunk.span.clone())
                    .await
                    .unwrap_or_else(|_| "<error displaying value>".to_string());
            Err(format!(
                "formatter returned non-string value: {display_str}"
            ))
        }
    }
}

/// Format source using the tinct-hosted formatter script at `script_path`.
///
/// The script receives the AST dict as `%` and must return a bare `String` on success.
/// To signal failure the script should call `raise`, which propagates as an EvalError.
/// Any non-String return value is a protocol error.
///
/// `script_path` is the path to a `.llt` formatter script (e.g. `stdlib/cli/fmt/pretty.llt`).
/// Whether to pass source/comment information is inferred from the script name:
/// scripts named `compact` receive a minimal AST (no source, no comments); all others
/// receive the full AST (with source info and comments for comment preservation).
///
/// Convenience wrapper: opens CWD ambiently. Callers that already hold an open
/// `cap_std::fs::Dir` should use [`format_source_tinct_with_dir`] instead.
pub async fn format_source_tinct(
    input: &str,
    script_path: &std::path::Path,
) -> Result<String, String> {
    format_source_tinct_with_dir(input, script_path, None).await
}
