// Source code pretty-printer. This module reformats LLT source for human reading.
//
// `tinct fmt` calls `format_source_tinct_with_dir`, which evaluates a tinct-hosted
// formatter script (`stdlib/cli/fmt/pretty.llt`) with the parsed AST dict as `%`.
// The Rust `Formatter` struct was deleted — it was only ever used by the now-removed
// `format_source` function that was dead code from `tinct fmt`'s perspective.
/// Format source using the tinct-hosted formatter script.
///
/// `script_source` is the already-read content of the formatter script (e.g. pretty.llt).
/// `script_name` is the name of the script file (used for compact-mode detection and
/// error messages). The caller is responsible for reading the script file using appropriate
/// capability-safe or bootstrap-phase filesystem access.
pub async fn format_source_tinct_with_dir(
    input: &str,
    script_source: &str,
    script_name: &str,
) -> Result<String, String> {
    use crate::desugar;
    use crate::error::DiagnosticLevel;
    use crate::eval::{self, EvalContext};
    use crate::parser::parse;
    use crate::resolve;
    use crate::typecheck;
    use crate::value::Value;
    use std::sync::Arc;

    // Determine mode from script name: compact.llt → minimal AST; everything else → full AST.
    let compact = std::path::Path::new(script_name)
        .file_stem()
        .and_then(|s| s.to_str())
        == Some("compact");

    // Parse the input source (no env/ctx needed yet).
    let file: Arc<str> = Arc::from("<formatter>");
    let parse_output = parse(input, file).map_err(|e| format!("{e}"))?;

    let formatter_file: Arc<str> = Arc::from(script_name);
    let formatter_parsed =
        parse(script_source, formatter_file).map_err(|e| format!("formatter parse error: {e}"))?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let formatter_program = desugar::desugar_program_full(&formatter_parsed.program);

    let ctx = EvalContext::new_empty();

    // Resolve the formatter program using the env-dict protocol.
    // "input-ast" is the formatter's own input variable name (not the loader's % convention).
    // The env-dict protocol assigns input-ast to LGM(root_group_len + 0), which is the slot
    // where eval_surface_file_with_input injects the AST thunk.
    let root_group_len = ctx.root_group.len() as u32;
    let mut resolve_diags: Vec<crate::error::TypeDiagnostic> = Vec::new();
    for doc_spanned in &formatter_program.documents {
        let (_table, diags, _unresolved) = resolve::resolve_surface_document_with_env_dict(
            &doc_spanned.node,
            &["input-ast".to_string()],
            root_group_len,
        );
        resolve_diags.extend(diags);
    }
    let resolve_errors: Vec<_> = resolve_diags
        .iter()
        .filter(|d| d.level == DiagnosticLevel::Err)
        .collect();
    if !resolve_errors.is_empty() {
        let msgs: Vec<String> = resolve_errors
            .iter()
            .map(|d| format!("resolve error: {}", d.message))
            .collect();
        return Err(msgs.join("\n"));
    }

    // Typecheck the desugared formatter (writes inline type annotations).
    let (tc_diags, _annotation_table, _tycon_env) =
        typecheck::typecheck_surface_program_annotation_table(&formatter_program).await;
    let tc_errors: Vec<_> = tc_diags
        .iter()
        .filter(|d| d.level == DiagnosticLevel::Err)
        .collect();
    if !tc_errors.is_empty() {
        let msgs: Vec<String> = tc_errors
            .iter()
            .map(|d| format!("type error: {}", d.message))
            .collect();
        return Err(msgs.join("\n"));
    }

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
                    .map_err(|e| {
                        format!("formatter returned non-string value (display failed: {e})")
                    })?;
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
/// `script_source` is the already-read content of the formatter script.
/// `script_name` is the name of the script file (e.g. `pretty.llt`). Scripts named
/// `compact` receive a minimal AST (no source, no comments); all others receive the
/// full AST (with source info and comments for comment preservation).
///
/// Alias for `format_source_tinct_with_dir`.
pub async fn format_source_tinct(
    input: &str,
    script_source: &str,
    script_name: &str,
) -> Result<String, String> {
    format_source_tinct_with_dir(input, script_source, script_name).await
}
