// Source code pretty-printer. This module reformats LLT source for human reading.
//
// `tinct fmt` calls `format_source_tinct_with_dir`, which evaluates a tinct-hosted
// formatter script (`stdlib/cli/fmt/pretty.llt`) with the parsed AST dict as `%`.
// The Rust `Formatter` struct was deleted — it was only ever used by the now-removed
// `format_source` function that was dead code from `tinct fmt`'s perspective.
/// Format source using the tinct-hosted formatter script.
pub async fn format_source_tinct_with_dir(
    input: &str,
    script_path: &std::path::Path,
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
    let formatter_program = desugar::desugar_program_full(&formatter_parsed.program);

    let ctx = EvalContext::new_empty();

    // Resolve the formatter program using the env-dict protocol.
    // "input-ast" is the formatter's own input variable name (not the loader's % convention).
    // The env-dict protocol assigns input-ast to LGM(root_group_len + 0), which is the slot
    // where eval_surface_file_with_input injects the AST thunk.
    let root_group_len = ctx.root_group.len() as u32;
    for doc_spanned in &formatter_program.documents {
        let _ = resolve::resolve_surface_document_with_env_dict(
            &doc_spanned.node,
            &["input-ast".to_string()],
            root_group_len,
        );
    }
    // Typecheck the desugared formatter (writes inline type annotations).
    let _ = typecheck::typecheck_surface_program_annotation_table(&formatter_program).await;

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
/// Alias for `format_source_tinct_with_dir`.
pub async fn format_source_tinct(
    input: &str,
    script_path: &std::path::Path,
) -> Result<String, String> {
    format_source_tinct_with_dir(input, script_path).await
}
