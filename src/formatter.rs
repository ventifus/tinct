// Source code pretty-printer. This module reformats LLT source for human reading.
//
// `tinct fmt` calls `format_source_tinct`, which evaluates a tinct-hosted
// formatter script (`stdlib/cli/fmt/pretty.llt`) with the parsed AST dict as `%`.
// The Rust `Formatter` struct was deleted — it was only ever used by the now-removed
// `format_source` function that was dead code from `tinct fmt`'s perspective.

use crate::expand;

/// The variable name that formatter scripts use to receive the parsed AST dict.
///
/// Formatter scripts access the parsed AST of the input source as `input-ast`.
/// This name is injected into the formatter script's evaluation environment via
/// the SymbolTable scope. The formatter script must bind its AST input to this
/// name — any custom formatter script must use `input-ast` as the variable that
/// receives the AST dict value passed by the Rust host.
///
/// See also: `doc/16b-rust-tinct-protocol.md §7` (Protocol Entry Points).
pub const FORMATTER_INPUT_VAR: &str = "input-ast";

/// Format source using the tinct-hosted formatter script.
///
/// `input` is the source text to format.
/// `script_source` is the already-read content of the formatter script (e.g. pretty.llt).
/// `script_name` is the name of the script file used in error messages.
/// `use_compact` selects between the minimal AST (`true`) and the full AST with source
/// and comment information (`false`). Pass `false` for normal formatting; pass `true`
/// only when the formatter script is known not to use comments or source positions.
///
/// The caller is responsible for reading the script file using appropriate
/// capability-safe or bootstrap-phase filesystem access.
pub async fn format_source_tinct(
    input: &str,
    script_source: &str,
    script_name: &str,
    use_compact: bool,
) -> Result<String, String> {
    use crate::desugar;
    use crate::error::DiagnosticLevel;
    use crate::eval::{self, EvalContext};
    use crate::parser::parse;
    use crate::resolve;
    use crate::symbol_table::SymbolTable;
    use crate::value::Value;
    use std::sync::Arc;

    // Parse the input source (no env/ctx needed yet).
    let file: Arc<str> = Arc::from("<formatter>");
    let (parse_result, _diagnostics) = parse(input, file).into_parts();
    let parse_output = parse_result.map_err(|e| format!("{e}"))?;

    let formatter_file: Arc<str> = Arc::from(script_name);
    let (formatter_result, _diagnostics) = parse(script_source, formatter_file).into_parts();
    let formatter_parsed = formatter_result.map_err(|e| format!("formatter parse error: {e}"))?;

    // PIPELINE INVARIANT: parse -> desugar -> expand -> resolve -> typecheck.
    let formatter_program = desugar::desugar_program_full(&formatter_parsed.program);
    let formatter_program = expand::expand_program(&formatter_program);

    let ctx = EvalContext::new_empty();

    // Build the scope for the formatter. FORMATTER_INPUT_VAR ("input-ast") is the
    // formatter's own input variable name (not the loader's % convention). It is
    // inserted into the scope so Field.resolution is always populated by the resolver.
    let scope = SymbolTable::empty_chain();

    // Resolve the formatter program against the scope.
    let mut resolve_diags: Vec<crate::error::Diagnostic> = Vec::new();
    for doc_spanned in &formatter_program.documents {
        let result = resolve::resolve_document(&doc_spanned.node, &scope, &Vec::new());
        resolve_diags.extend(result.diagnostics);
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

    // Typecheck the desugared formatter via the per-document pipeline.
    let mut pipeline_state = crate::pipeline::PipelineState::from_builtins(&ctx);
    for doc in &formatter_program.documents {
        crate::pipeline::process_pipeline_document(doc, &mut pipeline_state, &ctx)
            .await
            .take(&ctx.runtime_diagnostics)
            .map_err(|e| format!("formatter typecheck error: {e}"))?;
    }

    // Convert input AST to dict using the now-stable ctx.
    use crate::surface_convert::{surface_program_to_dict, AstToDictOpts, CommentMaps};
    let opts = if use_compact {
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
        surface_program_to_dict(&parse_output.program, &opts, &ctx).take(&ctx.runtime_diagnostics).map_err(|e| format!("{e}"))?;

    let mut input_scope = indexmap::IndexMap::new();
    input_scope.insert("%".to_string(), ast_thunk);
    let formatter_thunk = {
        let (result, _diags) = eval::eval_surface_file_with_input(&formatter_program, &ctx, input_scope)
            .await
            .into_parts();
        result.map_err(|e| format!("formatter eval error: {e}"))?
    };

    // Materialize the result — the formatter script must return a bare String.
    // If it raises (calls `raise`), the error propagates as an Diagnostic here.
    let result_val = {
        let (result, _diags) = eval::materialize(&formatter_thunk, None, &ctx)
            .await
            .into_parts();
        result.map_err(|e| format!("formatter error: {e}"))?
    };

    // Protocol: the formatter script returns a bare String on success.
    // Any other value is a protocol violation — the script should have returned
    // the string directly. If it needs to signal failure it should call `raise`.
    match result_val {
        Value::String {
            ref source,
            start,
            end,
            ..
        } => Ok(source[start..end].to_string()),

        _ => {
            let display_str = {
                let (result, diags) = crate::value_to_display_string(&result_val, &ctx, formatter_thunk.span.clone())
                    .await.into_parts();
                if !diags.is_empty() {
                    ctx.runtime_diagnostics.lock().unwrap().extend(diags);
                }
                result.map_err(|e| {
                    format!("formatter returned non-string value (display failed: {e})")
                })?
            };
            Err(format!(
                "formatter returned non-string value: {display_str}"
            ))
        }
    }
}
