// Source code pretty-printer. This module reformats LLT source for human reading.
//
// `tinct fmt` calls `format_source_tinct`, which evaluates a tinct-hosted
// formatter script (`stdlib/cli/fmt/pretty.llt`) with the parsed AST dict as `%`.
// The Rust `Formatter` struct was deleted — it was only ever used by the now-removed
// `format_source` function that was dead code from `tinct fmt`'s perspective.

/// The variable name that formatter scripts use to receive the parsed AST dict.
///
/// Formatter scripts access the parsed AST of the input source as `input-ast`.
/// This name is injected into the formatter script's evaluation environment via
/// `resolve_surface_document_with_seed_frames`. The formatter script must bind its
/// AST input to this name — any custom formatter script must use `input-ast` as
/// the variable that receives the AST dict value passed by the Rust host.
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
    use crate::typecheck;
    use crate::value::Value;
    use std::sync::Arc;

    // Parse the input source (no env/ctx needed yet).
    let file: Arc<str> = Arc::from("<formatter>");
    let parse_output = parse(input, file).map_err(|e| format!("{e}"))?;

    let formatter_file: Arc<str> = Arc::from(script_name);
    let formatter_parsed =
        parse(script_source, formatter_file).map_err(|e| format!("formatter parse error: {e}"))?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let formatter_program = desugar::desugar_program_full(&formatter_parsed.program);

    let ctx = EvalContext::new_empty();

    // Resolve the formatter program, seeding the resolver with root_group so that
    // builtins (including builtin-dict-get, used for dot-access) are in scope at their
    // correct runtime slots. FORMATTER_INPUT_VAR ("input-ast") is the formatter's own
    // input variable name (not the loader's % convention). It is appended after the seed
    // frames, so Field.resolution is always populated by the resolver — the lowerer never
    // needs to fall back to a scope_frames search.
    let root_group_len = ctx.root_group.len() as u32;
    let root_frame = ctx.root_group_resolver_map();
    let mut resolve_diags: Vec<crate::error::TypeDiagnostic> = Vec::new();
    for doc_spanned in &formatter_program.documents {
        let (_table, diags, _unresolved, _block_body_frames) =
            resolve::resolve_surface_document_with_seed_frames(
                &doc_spanned.node,
                &[root_frame.clone()],
                &[FORMATTER_INPUT_VAR.to_string()],
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
    let ts_data = crate::imports::get_builtin_core_type_stage_scope().await;
    let (tc_diags, _formatter_env, _tycon_env) = typecheck::typecheck_program_bootstrap(
        &formatter_program,
        std::sync::Arc::new(std::sync::RwLock::new(crate::env::Env::new())),
        None,
        std::collections::HashMap::new(),
        ts_data,
    )
    .await;
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
            ..
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
