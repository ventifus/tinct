// Source code pretty-printer. This module reformats LLT source for human reading,
// preserving structure while normalizing whitespace and indentation.
//
// **NOT for error messages.** For rendering an `Expr` node in a diagnostic, use
// `impl Display for Expr` in `src/ast.rs` — that is the normative representation
// for error output. This `Formatter` operates on the raw parse output (token spans)
// to reconstruct well-formatted source, not to describe what an expression means.
use std::sync::Arc;

use crate::ast::{
    Annotation, Spanned, SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceNamedArg,
    SurfaceNode, SurfaceParam,
};
use crate::parser::{parse, ParseError, ParseOutput};

pub fn format_source(input: &str) -> Result<String, ParseError> {
    let output = parse(input)?;
    let mut formatter = Formatter::new(&output, input, false, false);
    formatter.format_file();
    Ok(formatter.output)
}

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

    // Determine mode from script name: compact.llt → minimal AST; everything else → full AST.
    let compact = script_path.file_stem().and_then(|s| s.to_str()) == Some("compact");

    // Open the base directory first (needed by both expand_surface_program and EvalContext).
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
    let parse_output = parse(input).map_err(|e| format!("{e}"))?;

    // Load and expand the formatter script BEFORE creating env/ctx.
    // expand_surface_program internally calls create_stdlib_env_with_arena(), updating STDLIB_ARENA_CACHE.
    // By expanding first, env+ctx are created from the final cache state, keeping them
    // in the same arena basis and avoiding the "undefined variable: try" regression.
    // AMBIENT-OK: formatter script loaded from stdlib path.
    #[allow(clippy::disallowed_methods)]
    let formatter_source = std::fs::read_to_string(script_path).map_err(|e| {
        format!(
            "cannot read formatter script {}: {e}",
            script_path.display()
        )
    })?;
    let formatter_parsed =
        parse(&formatter_source).map_err(|e| format!("formatter parse error: {e}"))?;

    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    let mut formatter_program = formatter_parsed.program;
    let expand_result = crate::expand::expand_surface_program(
        &mut formatter_program,
        false, // formatter always has filesystem access
        &base_dir,
    )
    .await
    .map_err(|e| format!("formatter expand error: {e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    desugar::desugar_surface_program(&mut formatter_program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let formatter_resolution_table =
        std::sync::Arc::new(resolve::resolve_surface_program(&formatter_program));
    let formatter_type_annotation_table =
        std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());

    // Typecheck the expanded and desugared formatter.
    let _ = typecheck::typecheck_surface_program_annotation_table(&formatter_program);

    // Create env+ctx AFTER expand_surface_program so STDLIB_ARENA_CACHE is stable.
    // Use new_sharing_arena (not new) so stdlib ThunkIds are valid in the eval ctx —
    // same pattern as eval_source_with_config. With EvalContext::new (clone), ThunkIds
    // stored in prelude dicts are looked up in a separate arena vec and may be invalid.
    let (env, stdlib_arena) =
        crate::builtins::create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
    let type_stage_env = crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
    let ctx = EvalContext::new_sharing_arena(
        base_dir,
        Arc::clone(&env),
        type_stage_env,
        false,
        stdlib_arena,
        expand_result.macro_injects_map,
    );

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
    let formatter_thunk = eval::eval_surface_file_with_input(
        &formatter_program,
        Arc::clone(&env),
        &ctx,
        &formatter_resolution_table,
        &formatter_type_annotation_table,
        &std::collections::HashMap::new(),
        Some(ast_thunk),
    )
    .await
    .map_err(|e| format!("formatter eval error: {e}"))?;

    // Materialize the result — should be [Result.Ok String] or [Result.Error msg].
    let result_val = eval::materialize(&formatter_thunk, None, &ctx)
        .await
        .map_err(|e| format!("formatter materialize error: {e}"))?;

    // Unwrap [Result.Ok s] / surface [Result.Error msg].
    // The formatters return `[try [fn [] [format-file %]]]` which produces
    // Value::Variant { tag: "Result.Ok", payload: Some(string_thunk) } on success
    // or Value::Variant { tag: "Result.Error", payload: Some(msg_thunk) } on failure.
    match result_val {
        Value::Variant { tag, payload } if tag == "Result.Ok" => {
            let payload_id = payload.ok_or_else(|| "formatter Ok has no payload".to_string())?;
            let payload_thunk = ctx.get_thunk(payload_id);
            let ok_val = eval::materialize(&payload_thunk, None, &ctx)
                .await
                .map_err(|e| format!("formatter Ok materialize error: {e}"))?;
            match ok_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => Ok(source[start..end].to_string()),
                _ => {
                    let display_str =
                        crate::value_to_display_string(&ok_val, &ctx, payload_thunk.span.clone())
                            .unwrap_or_else(|_| "<error displaying value>".to_string());
                    Err(format!("formatter Ok value is not a string: {display_str}"))
                }
            }
        }
        Value::Variant { tag, payload } if tag == "Result.Error" => {
            let msg = if let Some(err_id) = payload {
                let err_thunk = ctx.get_thunk(err_id);
                let err_val = eval::materialize(&err_thunk, None, &ctx)
                    .await
                    .map_err(|e| format!("formatter Error materialize error: {e}"))?;
                crate::value_to_display_string(&err_val, &ctx, err_thunk.span.clone())
                    .unwrap_or_else(|_| "<error displaying value>".to_string())
            } else {
                "(no message)".to_string()
            };
            Err(format!("formatter error: {msg}"))
        }
        _ => {
            let display_str =
                crate::value_to_display_string(&result_val, &ctx, formatter_thunk.span.clone())
                    .unwrap_or_else(|_| "<error displaying value>".to_string());
            Err(format!(
                "formatter returned non-Result value: {display_str}"
            ))
        }
    }
}

/// Format source using the tinct-hosted formatter script at `script_path`.
///
/// The script receives the AST dict as `%` and must return `[Result.Ok String]` on success
/// or `[Result.Error msg]` on failure (both formatters use `[try [fn [] [format-file %]]]`).
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

struct Formatter<'a> {
    parse_output: &'a ParseOutput,
    source: &'a str,
    output: String,
    indent_level: usize,
    oneline: bool,
    nospaces: bool,
}

impl<'a> Formatter<'a> {
    fn new(parse_output: &'a ParseOutput, source: &'a str, oneline: bool, nospaces: bool) -> Self {
        Self {
            parse_output,
            source,
            output: String::with_capacity(source.len()),
            indent_level: 0,
            oneline,
            nospaces,
        }
    }

    fn format_file(&mut self) {
        for (i, doc) in self.parse_output.program.documents.iter().enumerate() {
            if i > 0 {
                // Document separator
                if self.oneline {
                    // In oneline mode, replace leading space with single space (or nothing if first doc),
                    // emit --- + header metadata as normal, then emit "; " instead of "\n\n"
                    if !self.output.is_empty() {
                        self.push_space(Some('-'));
                    }
                } else {
                    // Normal mode: ensure double newline before separator
                    if !self.output.is_empty() {
                        if !self.output.ends_with('\n') {
                            self.output.push('\n');
                        }
                        if !self.output.ends_with("\n\n") {
                            self.output.push('\n');
                        }
                    }
                }
                self.output.push_str("---");
                // Emit section header fields if present on this document.
                if let Some(ref name) = doc.node.name {
                    self.push_space(Some('%'));
                    self.output.push('%');
                    self.output.push_str(name);
                }
                if let Some(ref output_ann) = doc.node.output_type {
                    // Named section: %name@Type (no space before @).
                    // Unnamed section: --- @Type (space after --- already present from
                    // the section_name branch not running, so we add the space here).
                    if doc.node.name.is_none() {
                        self.push_space(Some('@'));
                    }
                    self.output.push('@');
                    self.format_annotation(output_ann);
                }
                if let Some(ref expects_ann) = doc.node.expects {
                    self.push_space(Some('e'));
                    self.output.push_str("expects:");
                    self.push_space(Some('@'));
                    self.output.push('@');
                    self.format_annotation(expects_ann);
                }
                if let Some(ref uses) = doc.node.uses {
                    self.push_space(Some('u'));
                    self.output.push_str("uses:");
                    self.push_space(Some('['));
                    self.output.push('[');
                    for (i, module) in uses.node.iter().enumerate() {
                        if i > 0 {
                            self.push_space(Some('"'));
                        }
                        self.output.push('"');
                        self.output.push_str(&module.node);
                        self.output.push('"');
                    }
                    self.output.push(']');
                }
                if self.oneline {
                    self.output.push(';');
                    // Push space before next document's first expression
                    let exprs: Vec<_> = doc.node.expressions().collect();
                    if !exprs.is_empty() {
                        self.push_space_before_expr(exprs[0]);
                    }
                } else {
                    self.output.push('\n');
                    self.output.push('\n');
                }
            }
            self.format_document(&doc.node);
        }
        self.ensure_trailing_newline();
    }

    fn format_document(&mut self, document: &SurfaceDocument) {
        let expressions: Vec<_> = document.expressions().collect();
        if expressions.is_empty() {
            return;
        }

        // In oneline mode, comments are always stripped and expressions are always on one line
        if self.oneline {
            for (i, expr) in expressions.iter().enumerate() {
                if i > 0 {
                    self.push_space_before_expr(expr);
                }
                self.format_expr_top_level(expr);
            }
            return;
        }

        // Decide if all expressions should be on one line:
        // True if all expressions are simple (not complex Dict/Call/Fn/TypeAlias)
        // AND there are no comments (which would require multi-line formatting)
        let all_simple = expressions.iter().all(|e| match &e.expr {
            SurfaceExpression::Dict(entries) if self.is_simple_dict(entries) => true,
            SurfaceExpression::Dict(_)
            | SurfaceExpression::Call { .. }
            | SurfaceExpression::Fn { .. }
            | SurfaceExpression::Sequential(_)
            | SurfaceExpression::Quote(_)
            | SurfaceExpression::Unquote(_)
            | SurfaceExpression::UnquoteSplice(_)
            | SurfaceExpression::Match { .. }
            | SurfaceExpression::LetDecl { .. }
            | SurfaceExpression::CaseArm { .. }
            | SurfaceExpression::Error(_) => false,
            _ => true,
        });

        let has_comments = expressions.iter().any(|e| {
            self.parse_output
                .leading_comments
                .contains_key(&e.span.start.offset)
                || self
                    .parse_output
                    .trailing_comments
                    .contains_key(&e.span.start.offset)
        });

        if all_simple && !has_comments {
            // Format all on one line with spaces
            for (i, expr) in expressions.iter().enumerate() {
                if i > 0 {
                    self.push_space_before_expr(expr);
                }
                self.format_expr_top_level(expr);
            }
        } else {
            // Format each expression on its own line
            for (i, expr) in expressions.iter().enumerate() {
                let has_leading_comment = self
                    .parse_output
                    .leading_comments
                    .contains_key(&expr.span.start.offset);

                // Leading comments
                if let Some(comments) = self
                    .parse_output
                    .leading_comments
                    .get(&expr.span.start.offset)
                {
                    if i > 0 {
                        self.push_newline();
                    }
                    for comment in comments {
                        self.write_indent();
                        self.output.push('#');
                        self.output.push_str(comment);
                        self.push_newline();
                    }
                }

                // Blank line before non-first expression (but not if there's a leading comment)
                if i > 0 && !has_leading_comment {
                    self.push_newline();
                }

                self.format_expr_top_level(expr);

                // Trailing comments
                if let Some(comment) = self
                    .parse_output
                    .trailing_comments
                    .get(&expr.span.start.offset)
                {
                    self.push_space(Some('#'));
                    self.output.push('#');
                    self.output.push_str(comment);
                }

                self.push_newline();
            }
        }
    }

    fn format_expr_top_level(&mut self, expr: &Arc<SurfaceNode>) {
        self.format_expr(expr, false);
    }

    fn format_expr(&mut self, expr: &Arc<SurfaceNode>, _in_bracket: bool) {
        match &expr.expr {
            SurfaceExpression::Int(n) => self.output.push_str(&n.to_string()),
            SurfaceExpression::U64(n) => {
                self.output.push_str(&n.to_string());
                self.output.push('u');
            }
            SurfaceExpression::Float(f) => {
                let s = f.to_string();
                self.output.push_str(&s);
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    self.output.push_str(".0");
                }
            }
            SurfaceExpression::Bool(b) => self.output.push_str(if *b { "true" } else { "false" }),
            SurfaceExpression::Str(s) => {
                // Source-sniff: check if the original token was a quoted string or a
                // bare identifier (dict key). If the source character at the span start
                // is `"`, emit with quotes; otherwise emit bare (e.g. `x` in `[x: 1]`).
                let is_quoted = self
                    .source
                    .as_bytes()
                    .get(expr.span.start.offset)
                    .is_some_and(|&b| b == b'"');
                if is_quoted {
                    self.output.push('"');
                    for ch in s.chars() {
                        match ch {
                            '"' => self.output.push_str("\\\""),
                            '\\' => self.output.push_str("\\\\"),
                            '\n' => self.output.push_str("\\n"),
                            '\t' => self.output.push_str("\\t"),
                            '\r' => self.output.push_str("\\r"),
                            _ => self.output.push(ch),
                        }
                    }
                    self.output.push('"');
                } else {
                    // Bare identifier key — emit without quotes
                    self.output.push_str(s);
                }
            }
            SurfaceExpression::VarRef { name, escaped } => {
                // Emit `$` prefix if this was written as an escaped ref (`$name`).
                // Bare identifiers and `%`-prefixed refs do not get a `$` prepended —
                // the `%` is already part of `name`.
                if *escaped {
                    self.output.push('$');
                }
                self.output.push_str(name);
            }
            SurfaceExpression::DotAccess { expr, field } => {
                self.format_expr(expr, false);
                self.output.push('.');
                match field {
                    crate::ast::DotKey::Ident(s) => self.output.push_str(s),
                    crate::ast::DotKey::Int(n) => self.output.push_str(&n.to_string()),
                }
            }
            SurfaceExpression::Pipe { lhs, rhs } => {
                self.format_expr(lhs, false);
                self.push_space(Some('|'));
                self.output.push('|');
                self.push_space_before_expr(rhs);
                self.format_expr(rhs, false);
            }
            SurfaceExpression::Sequential(exprs) => {
                // Format sequential expressions as a pseudo-list.
                // This is a synthetic node created by let-binding, not user-written syntax.
                // Display as (seq expr1 expr2 ...) for debugging.
                self.output.push_str("(seq");
                for seq_expr in exprs {
                    self.push_space(None);
                    self.format_expr(seq_expr, false);
                }
                self.output.push(')');
            }
            SurfaceExpression::Dict(entries) => self.format_dict(entries),
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            } => self.format_call(func, args, named_args, *implied),
            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared: _,
            } => self.format_fn(return_ann, params, body),
            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
                self.output.push('[');
                self.output.push('@');
                self.format_annotation(annotation);
                self.push_space_before_expr(expr);
                self.format_expr(expr, true);
                self.output.push(']');
            }
            SurfaceExpression::Annotated { name, annotation } => {
                self.output.push_str(name);
                self.output.push('@');
                self.format_annotation(annotation);
            }
            SurfaceExpression::Quote(inner) => {
                self.output.push('[');
                self.output.push_str("quote");
                self.push_space_before_expr(inner);
                self.format_expr(inner, true);
                self.output.push(']');
            }
            SurfaceExpression::Unquote(inner) => {
                self.output.push('[');
                self.output.push_str("unquote");
                self.push_space_before_expr(inner);
                self.format_expr(inner, true);
                self.output.push(']');
            }
            SurfaceExpression::UnquoteSplice(inner) => {
                self.output.push('[');
                self.output.push_str("unquote-splice");
                self.push_space_before_expr(inner);
                self.format_expr(inner, true);
                self.output.push(']');
            }
            // MacroDecl, Splice, SyntaxClass, TypeAlias, ClassDecl, InstanceDecl
            // are SurfaceDeclaration variants, not SurfaceExpression variants.
            // They are filtered out by document.expressions() and never reach format_expr.
            SurfaceExpression::Match { scrutinee, arms } => {
                self.output.push('[');
                self.output.push_str("match");
                self.push_space_before_expr(scrutinee);
                self.format_expr(scrutinee, true);
                for arm in arms {
                    self.output.push(' ');
                    self.format_pattern(&arm.pattern);
                    self.output.push(':');
                    // Handle multi-body (Sequential) in arm bodies
                    if let SurfaceExpression::Sequential(body_exprs) = &arm.body.expr {
                        for body_expr in body_exprs {
                            self.push_space_before_expr(body_expr);
                            self.format_expr(body_expr, true);
                        }
                    } else {
                        self.push_space_before_expr(&arm.body);
                        self.format_expr(&arm.body, true);
                    }
                }
                self.output.push(']');
            }
            SurfaceExpression::PatternDecl { bindings } => {
                self.output.push('[');
                self.output.push_str("pattern");
                self.output.push_str(" [");
                for (i, binding) in bindings.iter().enumerate() {
                    if i > 0 {
                        self.output.push(' ');
                    }
                    self.format_expr(binding, false);
                }
                self.output.push_str("]]");
            }
            SurfaceExpression::LetDecl { bindings } => {
                self.output.push('[');
                self.output.push_str("let");
                for binding in bindings.iter() {
                    self.output.push(' ');
                    self.format_expr(binding, false);
                }
                self.output.push(']');
            }
            SurfaceExpression::CaseArm { pattern, body } => {
                self.output.push('[');
                self.output.push_str("case");
                self.output.push(' ');
                self.format_expr(pattern, false);
                self.output.push(' ');
                self.format_expr(body, false);
                self.output.push(']');
            }
            SurfaceExpression::Placeholder => {
                self.output.push_str("...");
            }
            SurfaceExpression::Decl(_) => {
                // Declaration embedded in expression context: format as Placeholder.
                // The actual declaration content is preserved for type checking only.
                self.output.push_str("...");
            }
            SurfaceExpression::Rest(name) => {
                self.output.push_str("...");
                if let Some(n) = name {
                    self.output.push_str(n);
                }
            }
            SurfaceExpression::Error(span) => {
                // Emit original source text verbatim for error nodes
                let text = &self.source[span.start.offset..span.end.offset];
                self.output.push_str(text);
            }
        }
    }

    fn format_dict(&mut self, entries: &[Spanned<SurfaceEntry>]) {
        if entries.is_empty() {
            self.output.push_str("[]");
            return;
        }

        // Decide single-line vs multi-line
        let has_comments = self.dict_has_comments(entries);
        let entry_count = entries.len();
        // Add indent_level * 2 here to account for the column at which this dict begins.
        // This is the correct place to apply the line-start offset: format_dict is called
        // at a known indentation level and needs to know the total occupied column width.
        // measure_dict_width itself must NOT include this offset, because it is also called
        // recursively from measure_expr_width where the indent offset would double-count.
        let single_line_width = self.measure_dict_width(entries) + self.indent_level * 2;

        let use_single_line = if has_comments {
            false
        } else {
            entry_count <= 4 && single_line_width <= 80
        };

        if use_single_line {
            self.format_dict_single_line(entries);
        } else {
            self.format_dict_multi_line(entries);
        }
    }

    fn is_simple_dict(&self, entries: &[Spanned<SurfaceEntry>]) -> bool {
        // A simple dict is one that would fit on a single line (4 or fewer entries, width <= 80).
        // Add indent_level * 2 here to account for the column at which this dict begins,
        // mirroring the same adjustment in format_dict. measure_dict_width itself does not
        // include this offset so that recursive calls from measure_expr_width are not affected.
        let has_comments = self.dict_has_comments(entries);
        let entry_count = entries.len();
        let single_line_width = self.measure_dict_width(entries) + self.indent_level * 2;
        !has_comments && entry_count <= 4 && single_line_width <= 80
    }

    fn dict_has_comments(&self, entries: &[Spanned<SurfaceEntry>]) -> bool {
        for entry in entries {
            // Check for leading comments at the entry start
            if self
                .parse_output
                .leading_comments
                .contains_key(&entry.span.start.offset)
            {
                return true;
            }
            // Check for trailing comments at the value end (parser keys them by the last token)
            if self
                .parse_output
                .trailing_comments
                .contains_key(&entry.node.value.span.start.offset)
            {
                return true;
            }
        }
        false
    }

    fn measure_dict_width(&self, entries: &[Spanned<SurfaceEntry>]) -> usize {
        let mut width = 2; // [ and ]
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                width += 1; // space
            }
            if let Some(key) = &entry.node.key {
                width += self.measure_expr_width(key);
                width += 2; // ": "
            }
            width += self.measure_expr_width(&entry.node.value);
        }
        width
    }

    fn measure_expr_width(&self, expr: &Arc<SurfaceNode>) -> usize {
        match &expr.expr {
            SurfaceExpression::Int(n) => {
                if *n < 0 {
                    1 + (-n).to_string().len()
                } else {
                    n.to_string().len()
                }
            }
            SurfaceExpression::U64(n) => n.to_string().len() + 1, // trailing 'u'
            SurfaceExpression::Float(f) => {
                let s = f.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    s.len() + 2 // appended ".0"
                } else {
                    s.len()
                }
            }
            SurfaceExpression::Bool(b) => {
                if *b {
                    4
                } else {
                    5
                }
            }
            SurfaceExpression::Str(s) => {
                // Source-sniff: if originally a quoted string, add 2 for the quote characters.
                // If originally a bare identifier key, width is just the content length.
                let is_quoted = self
                    .source
                    .as_bytes()
                    .get(expr.span.start.offset)
                    .is_some_and(|&b| b == b'"');
                if is_quoted {
                    s.len() + 2
                } else {
                    s.len()
                }
            }
            SurfaceExpression::VarRef { name, escaped, .. } => {
                // Add 1 for `$` if this was an escaped ref.
                // `%`-prefixed refs already include `%` in the stored name.
                name.len() + if *escaped { 1 } else { 0 }
            }
            SurfaceExpression::DotAccess { expr, field } => {
                let field_len = match field {
                    crate::ast::DotKey::Ident(s) => s.len(),
                    crate::ast::DotKey::Int(n) => n.to_string().len(),
                };
                self.measure_expr_width(expr) + 1 + field_len
            }
            SurfaceExpression::Pipe { lhs, rhs } => {
                self.measure_expr_width(lhs) + 3 + self.measure_expr_width(rhs) // lhs | rhs
            }
            SurfaceExpression::Sequential(exprs) => {
                // Measure as (seq expr1 expr2 ...)
                let mut width = 4; // "(seq"
                for seq_expr in exprs {
                    width += 1 + self.measure_expr_width(seq_expr);
                }
                width + 1 // closing ")"
            }
            SurfaceExpression::Dict(entries) => self.measure_dict_width(entries),
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                // Mirror format_call: if this would be formatted as i"...", measure that width.
                if named_args.is_empty() && self.is_interpolated_string_call(func, args) {
                    // i" + content + "
                    let mut w = 3; // i""
                    for arg in args {
                        match &arg.expr {
                            SurfaceExpression::Str(s) => w += s.len(), // approximate (escaping may expand)
                            SurfaceExpression::VarRef { name, .. } => w += 1 + name.len(), // $name
                            _ => {}
                        }
                    }
                    return w;
                }
                let mut w = 1; // [
                if !*implied {
                    w += 4 + 1; // call
                }
                w += self.measure_expr_width(func);
                for arg in args {
                    w += 1 + self.measure_expr_width(arg);
                }
                for named_arg in named_args {
                    w += 1
                        + named_arg.node.name.len()
                        + 2
                        + self.measure_expr_width(&named_arg.node.value);
                }
                w + 1 // ]
            }
            SurfaceExpression::Fn { params, body, .. } => {
                let mut w = 1 + 2 + 1; // [fn ]
                w += 1 + 3; // [let
                for param in params.iter() {
                    w += 1; // space before each param
                    w += param.node.name.len();
                    if param.node.variadic {
                        w += 3; // ...
                    }
                    if let Some(ann) = &param.node.annotation {
                        w += 1 + self.measure_annotation_width(&ann.node);
                    }
                }
                w += 1; // ]
                w += 1; // space
                w += self.measure_expr_width(body);
                w + 1 // ]
            }
            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
                1 + 1
                    + self.measure_annotation_width(&annotation.node)
                    + 1
                    + self.measure_expr_width(expr)
                    + 1
            }
            SurfaceExpression::Annotated { name, annotation } => {
                name.len() + 1 + self.measure_annotation_width(&annotation.node)
            }
            SurfaceExpression::Quote(inner) => 1 + 5 + 1 + self.measure_expr_width(inner) + 1, // [quote <expr>]
            SurfaceExpression::Unquote(inner) => 1 + 7 + 1 + self.measure_expr_width(inner) + 1, // [unquote <expr>]
            SurfaceExpression::UnquoteSplice(inner) => {
                1 + 14 + 1 + self.measure_expr_width(inner) + 1
            } // [unquote-splice <expr>]
            SurfaceExpression::Match { scrutinee, arms } => {
                let mut width = 1 + 5 + 1 + self.measure_expr_width(scrutinee); // [match <scrutinee>
                for arm in arms {
                    width += 1 + self.measure_pattern_width(&arm.pattern.node) + 1; // <space><pattern>:
                                                                                    // Handle multi-body (Sequential) in arm bodies
                    if let SurfaceExpression::Sequential(body_exprs) = &arm.body.expr {
                        for body_expr in body_exprs {
                            width += 1 + self.measure_expr_width(body_expr);
                        }
                    } else {
                        width += 1 + self.measure_expr_width(&arm.body);
                    }
                }
                width + 1 // closing ]
            }
            SurfaceExpression::PatternDecl { bindings } => {
                let mut width = 1 + 7 + 2; // [pattern [
                for (i, binding) in bindings.iter().enumerate() {
                    if i > 0 {
                        width += 1;
                    }
                    width += self.measure_expr_width(binding);
                }
                width + 2 // ]]
            }
            SurfaceExpression::LetDecl { bindings } => {
                let mut width = 1 + 3; // [let
                for binding in bindings.iter() {
                    width += 1 + self.measure_expr_width(binding);
                }
                width + 1 // ]
            }
            SurfaceExpression::CaseArm { pattern, body } => {
                1 + 4 + 1 + self.measure_expr_width(pattern) + 1 + self.measure_expr_width(body) + 1
                // [case <pattern> <body>]
            }
            SurfaceExpression::Placeholder | SurfaceExpression::Decl(_) => 3, // ...
            SurfaceExpression::Rest(name) => 3 + name.as_ref().map_or(0, |n| n.len()),
            SurfaceExpression::Error(span) => {
                // Measure the width of the original source text
                span.end.offset - span.start.offset
            }
        }
    }

    fn measure_pattern_width(&self, pattern: &crate::ast::Pattern) -> usize {
        use crate::ast::{LiteralPattern, Pattern};
        match pattern {
            Pattern::Wildcard => 1,
            Pattern::Variable(name) => name.len(),
            Pattern::TypeTag(tag) => tag.len(),
            Pattern::Pin(name) => 1 + name.len(), // $name
            Pattern::TypeAssertPending { annotation, inner } => {
                // [@Ann inner] or [@Ann] — 3 for "[@" + "]", annotation width, optional inner
                let ann_width = format!("{}", annotation.node).len();
                let inner_width = match inner {
                    Some(inner_pat) => 1 + self.measure_pattern_width(&inner_pat.node), // " inner"
                    None => 0,
                };
                3 + ann_width + inner_width
            }
            Pattern::TypeAssert { inner, .. } => {
                // Treat as wildcard width estimate for formatting purposes
                let inner_width = match inner {
                    Some(inner_pat) => 1 + self.measure_pattern_width(&inner_pat.node),
                    None => 0,
                };
                12 + inner_width // "[@<resolved>]" is 13 chars
            }
            Pattern::Literal(lit) => match lit {
                LiteralPattern::Int(n) => n.to_string().len(),
                LiteralPattern::U64(n) => n.to_string().len() + 1, // trailing 'u'
                LiteralPattern::Float(f) => {
                    let s = f.to_string();
                    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                        s.len() + 2 // appended ".0"
                    } else {
                        s.len()
                    }
                }
                LiteralPattern::Bool(b) => b.to_string().len(),
                LiteralPattern::Str(s) => 2 + s.len(), // "string" (approximate, doesn't account for escapes)
            },
            Pattern::Dict { fields, rest } => {
                let mut width = 2; // []
                for (i, (key, pat)) in fields.iter().enumerate() {
                    if i > 0 {
                        width += 2; // "  "
                    }
                    width += key.len() + 2; // "key: "
                    width += self.measure_pattern_width(&pat.node);
                }
                if *rest {
                    if !fields.is_empty() {
                        width += 2; // "  "
                    }
                    width += 3; // "..."
                }
                width
            }
            Pattern::Seq { head, tail } => {
                6 + self.measure_pattern_width(&head.node) + self.measure_pattern_width(&tail.node)
                // "[seq h t]"
            }
            Pattern::Constructor { tag, binding } => {
                if let Some(pat) = binding {
                    2 + tag.len() + 1 + self.measure_pattern_width(&pat.node) // "[Tag pat]"
                } else {
                    tag.len() // "Tag"
                }
            }
            Pattern::Or(patterns) => {
                let mut width = 0;
                for (i, pat) in patterns.iter().enumerate() {
                    if i > 0 {
                        width += 3; // " | "
                    }
                    width += self.measure_pattern_width(&pat.node);
                }
                width
            }
        }
    }

    fn measure_annotation_width(&self, annotation: &Annotation) -> usize {
        match annotation {
            Annotation::Simple(name) => name.len(),
            Annotation::PropertyDict(entries) => self.measure_annotation_dict_width(entries),
            Annotation::Annotated(name, inner) => {
                name.len() + 1 + self.measure_annotation_width(inner) // name + @ + inner
            }
        }
    }

    /// Measure the formatted width of a `PropertyDict` annotation entry list.
    /// Uses `SurfaceNode::Display` to render keys and values.
    fn measure_annotation_dict_width(&self, entries: &[Spanned<SurfaceEntry>]) -> usize {
        let mut width = 2; // [ and ]
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                width += 1; // space
            }
            if let Some(key) = &entry.node.key {
                width += format!("{key}").len();
                width += 2; // ": "
            }
            width += format!("{}", entry.node.value).len();
        }
        width
    }

    /// Format a `PropertyDict` annotation entry list as a bracketed dict.
    /// Uses `SurfaceNode::Display` to render keys and values.
    fn format_annotation_dict(&mut self, entries: &[Spanned<SurfaceEntry>]) {
        if entries.is_empty() {
            self.output.push_str("[]");
            return;
        }
        let width = self.measure_annotation_dict_width(entries) + self.indent_level * 2;
        let use_single_line = entries.len() <= 4 && width <= 80;
        self.output.push('[');
        if use_single_line {
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    self.output.push(' ');
                }
                if let Some(key) = &entry.node.key {
                    self.output.push_str(&format!("{key}"));
                    self.output.push_str(": ");
                }
                self.output.push_str(&format!("{}", entry.node.value));
            }
        } else {
            self.indent_level += 1;
            for entry in entries {
                self.push_newline();
                self.write_indent();
                if let Some(key) = &entry.node.key {
                    self.output.push_str(&format!("{key}"));
                    self.output.push_str(": ");
                }
                self.output.push_str(&format!("{}", entry.node.value));
            }
            self.indent_level -= 1;
            self.push_newline();
            self.write_indent();
        }
        self.output.push(']');
    }

    fn format_dict_single_line(&mut self, entries: &[Spanned<SurfaceEntry>]) {
        self.output.push('[');
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                self.push_space_before_expr(&entry.node.value);
            }
            if let Some(key) = &entry.node.key {
                self.format_expr(key, true);
                self.output.push(':');
                self.push_space_before_expr(&entry.node.value);
            }
            self.format_expr(&entry.node.value, true);
        }
        self.output.push(']');
    }

    fn format_dict_multi_line(&mut self, entries: &[Spanned<SurfaceEntry>]) {
        self.output.push('[');
        self.indent_level += 1;

        for entry in entries {
            // Leading comments for this entry
            if let Some(comments) = self
                .parse_output
                .leading_comments
                .get(&entry.span.start.offset)
            {
                for comment in comments {
                    self.push_newline();
                    self.write_indent();
                    self.output.push('#');
                    self.output.push_str(comment);
                }
            }

            self.push_newline();
            self.write_indent();
            if let Some(key) = &entry.node.key {
                self.format_expr(key, true);
                self.output.push(':');
                self.push_space_before_expr(&entry.node.value);
            }
            self.format_expr(&entry.node.value, true);

            // Trailing comments for this entry (keyed by value offset)
            if let Some(comment) = self
                .parse_output
                .trailing_comments
                .get(&entry.node.value.span.start.offset)
            {
                self.push_space(Some('#'));
                self.output.push('#');
                self.output.push_str(comment);
            }
        }

        self.indent_level -= 1;
        self.push_newline();
        self.write_indent();
        self.output.push(']');
    }

    fn format_call(
        &mut self,
        func: &Arc<SurfaceNode>,
        args: &[Arc<SurfaceNode>],
        named_args: &[Spanned<SurfaceNamedArg>],
        implied: bool,
    ) {
        // If this looks like a desugared i"..." call (head is "str", no named args,
        // all args are Str literals or VarRef nodes), format as i"..." instead of [str ...].
        // This is heuristic: user-written [str ...] calls matching the pattern will also
        // round-trip as i"..." syntax, which is acceptable for a pre-1.0 formatter.
        if named_args.is_empty() && self.is_interpolated_string_call(func, args) {
            self.format_as_interpolated_string(args);
            return;
        }

        self.output.push('[');
        if !implied {
            self.output.push_str("call");
            self.push_space_before_expr(func);
        }
        self.format_expr(func, true);
        for arg in args {
            self.push_space_before_expr(arg);
            self.format_expr(arg, true);
        }
        for named_arg in named_args {
            self.push_space(named_arg.node.name.chars().next());
            self.output.push_str(&named_arg.node.name);
            self.output.push(':');
            self.push_space_before_expr(&named_arg.node.value);
            self.format_expr(&named_arg.node.value, true);
        }
        self.output.push(']');
    }

    /// Return true if this Call node matches the pattern produced by emit_tmpl_call.
    ///
    /// Pattern: func is a VarRef named "tmpl", no named args, and args[0] is a Str
    /// (the raw template). Additional args[1..] are the expression args for ${N} slots.
    fn is_interpolated_string_call(
        &self,
        func: &Arc<SurfaceNode>,
        args: &[Arc<SurfaceNode>],
    ) -> bool {
        // Head must be exactly VarRef("tmpl")
        let SurfaceExpression::VarRef { name, .. } = &func.expr else {
            return false;
        };
        if name != "tmpl" {
            return false;
        }
        // Must have at least one arg: the raw template string
        if args.is_empty() {
            return false;
        }
        // args[0] must be a Str (the raw template)
        matches!(&args[0].expr, SurfaceExpression::Str(_))
    }

    /// Format a [tmpl "raw-template" expr0 ...] call as i"..." syntax.
    ///
    /// The raw template uses: `$$` for literal `$`, `$name` for variable refs,
    /// `${N}` for expression args (where N is 0-based index into args[1..]).
    fn format_as_interpolated_string(&mut self, args: &[Arc<SurfaceNode>]) {
        let raw = match &args[0].expr {
            SurfaceExpression::Str(s) => s.clone(),
            _ => return, // Should never happen given is_interpolated_string_call guard
        };
        let expr_args = &args[1..];

        self.output.push_str("i\"");

        let chars: Vec<char> = raw.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '$' {
                i += 1;
                if i >= chars.len() {
                    // Trailing $ — emit as-is (shouldn't happen in valid templates)
                    self.output.push('$');
                } else if chars[i] == '$' {
                    // $$ → literal $, emit as $$
                    self.output.push_str("$$");
                    i += 1;
                } else if chars[i] == '{' {
                    // ${N} → expression placeholder; find closing }
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '}' {
                        i += 1;
                    }
                    let idx_str: String = chars[start..i].iter().collect();
                    i += 1; // skip '}'
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if let Some(expr_arg) = expr_args.get(idx) {
                            // Format the expression inline: ${[expr]}
                            self.output.push_str("${");
                            self.format_expr(expr_arg, false);
                            self.output.push('}');
                        }
                    }
                } else {
                    // $name → variable reference
                    let start = i;
                    while i < chars.len() && !is_tmpl_stop_char(chars[i]) {
                        i += 1;
                    }
                    let var_name: String = chars[start..i].iter().collect();
                    self.output.push('$');
                    self.output.push_str(&var_name);
                }
            } else {
                // Literal character — escape special chars for i"..." context
                match chars[i] {
                    '"' => self.output.push_str("\\\""),
                    '\\' => self.output.push_str("\\\\"),
                    '\n' => self.output.push_str("\\n"),
                    '\t' => self.output.push_str("\\t"),
                    '\r' => self.output.push_str("\\r"),
                    c => self.output.push(c),
                }
                i += 1;
            }
        }
        self.output.push('"');
    }

    fn format_fn(
        &mut self,
        return_ann: &Option<Spanned<Annotation>>,
        params: &[Spanned<SurfaceParam>],
        body: &Arc<SurfaceNode>,
    ) {
        self.output.push('[');
        self.output.push_str("fn");
        if let Some(ann) = return_ann {
            self.output.push('@');
            self.format_annotation(ann);
        }
        self.push_space(Some('['));

        // Params always single-line; emit [let ...] form
        self.output.push('[');
        self.output.push_str("let");
        for param in params.iter() {
            let first_char = if param.node.variadic {
                Some('.')
            } else {
                param.node.name.chars().next()
            };
            self.push_space(first_char);
            if param.node.variadic {
                self.output.push_str("...");
            }
            self.output.push_str(&param.node.name);
            if let Some(ann) = &param.node.annotation {
                self.output.push('@');
                self.format_annotation(ann);
            }
        }
        self.output.push(']');

        self.push_space_before_expr(body);
        self.format_expr(body, true);
        self.output.push(']');
    }

    fn format_annotation(&mut self, annotation: &Spanned<Annotation>) {
        match &annotation.node {
            Annotation::Simple(name) => self.output.push_str(name),
            Annotation::PropertyDict(entries) => {
                // Format as a dict bracket using the old Entry type (annotations were not migrated)
                self.format_annotation_dict(entries);
            }
            Annotation::Annotated(name, inner) => {
                self.output.push_str(name);
                self.output.push('@');
                // Create a temporary Spanned wrapper for the inner annotation
                let inner_spanned = Spanned::new(inner.as_ref().clone(), annotation.span.clone());
                self.format_annotation(&inner_spanned);
            }
        }
    }

    fn format_pattern(&mut self, pattern: &Spanned<crate::ast::Pattern>) {
        use crate::ast::{LiteralPattern, Pattern};
        match &pattern.node {
            Pattern::Wildcard => self.output.push('_'),
            Pattern::Variable(name) => self.output.push_str(name),
            Pattern::TypeTag(tag) => self.output.push_str(tag),
            Pattern::Pin(name) => {
                self.output.push('$');
                self.output.push_str(name);
            }
            Pattern::Literal(lit) => match lit {
                LiteralPattern::Int(n) => self.output.push_str(&n.to_string()),
                LiteralPattern::U64(n) => {
                    self.output.push_str(&n.to_string());
                    self.output.push('u');
                }
                LiteralPattern::Float(f) => {
                    let s = f.to_string();
                    self.output.push_str(&s);
                    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                        self.output.push_str(".0");
                    }
                }
                LiteralPattern::Bool(b) => self.output.push_str(&b.to_string()),
                LiteralPattern::Str(s) => {
                    self.output.push('"');
                    self.output
                        .push_str(&s.replace('\\', "\\\\").replace('"', "\\\""));
                    self.output.push('"');
                }
            },
            Pattern::Dict { fields, rest } => {
                self.output.push('[');
                for (i, (key, pat)) in fields.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str("  ");
                    }
                    self.output.push_str(key);
                    self.output.push_str(": ");
                    self.format_pattern(pat);
                }
                if *rest {
                    if !fields.is_empty() {
                        self.output.push_str("  ");
                    }
                    self.output.push_str("...");
                }
                self.output.push(']');
            }
            Pattern::Seq { head, tail } => {
                self.output.push_str("[seq ");
                self.format_pattern(head);
                self.output.push(' ');
                self.format_pattern(tail);
                self.output.push(']');
            }
            Pattern::Constructor { tag, binding } => {
                if let Some(pat) = binding {
                    self.output.push('[');
                    self.output.push_str(tag);
                    self.output.push(' ');
                    self.format_pattern(pat);
                    self.output.push(']');
                } else {
                    self.output.push_str(tag);
                }
            }
            Pattern::TypeAssertPending { annotation, inner } => {
                self.output.push_str("[@");
                self.format_annotation(annotation);
                if let Some(inner_pat) = inner {
                    self.output.push(' ');
                    self.format_pattern(inner_pat);
                }
                self.output.push(']');
            }
            Pattern::TypeAssert { inner, .. } => {
                // Post-elaboration form: format as placeholder (not user-visible in normal code)
                self.output.push_str("[@<resolved>");
                if let Some(inner_pat) = inner {
                    self.output.push(' ');
                    self.format_pattern(inner_pat);
                }
                self.output.push(']');
            }
            Pattern::Or(patterns) => {
                for (i, pat) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(" | ");
                    }
                    self.format_pattern(pat);
                }
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("  ");
        }
    }

    fn ensure_trailing_newline(&mut self) {
        if !self.oneline && !self.output.ends_with('\n') {
            self.output.push('\n');
        }
    }

    /// Push a space, but only if required by the `nospaces` mode.
    /// When `nospaces` is true, only insert a space if both the last character of the output
    /// and the next character to be written are bare-word characters (alphanumeric, -, _, ?, !, /, %, ~).
    /// The next_char parameter should be the first character of the next token (or None if unknown).
    fn push_space(&mut self, next_char: Option<char>) {
        if self.nospaces {
            // Only insert space if both preceding and following chars are bare-word chars
            let last_is_bareword = self.output.chars().last().is_some_and(is_bare_word_char);
            let next_is_bareword = next_char.is_some_and(is_bare_word_char);
            if last_is_bareword && next_is_bareword {
                self.output.push(' ');
            }
        } else {
            self.output.push(' ');
        }
    }

    /// Push a newline, or a space if in `oneline` mode.
    fn push_newline(&mut self) {
        if self.oneline {
            self.output.push(' ');
        } else {
            self.output.push('\n');
        }
    }

    /// Push a space before an expression, considering the nospaces mode.
    fn push_space_before_expr(&mut self, expr: &Arc<SurfaceNode>) {
        let first_char = self.first_char_of_expr(expr);
        self.push_space(first_char);
    }

    /// Get the first character that will be emitted when formatting this expression.
    fn first_char_of_expr(&self, expr: &Arc<SurfaceNode>) -> Option<char> {
        match &expr.expr {
            SurfaceExpression::Int(n) => {
                if *n < 0 {
                    Some('-')
                } else {
                    n.to_string().chars().next()
                }
            }
            SurfaceExpression::U64(n) => n.to_string().chars().next(),
            SurfaceExpression::Float(_) => Some('0'), // approximate
            SurfaceExpression::Bool(b) => Some(if *b { 't' } else { 'f' }),
            SurfaceExpression::Str(_) => {
                // Check if quoted or bare
                let is_quoted = self
                    .source
                    .as_bytes()
                    .get(expr.span.start.offset)
                    .is_some_and(|&b| b == b'"');
                if is_quoted {
                    Some('"')
                } else {
                    // Bare string - first char of the string content
                    Some('a') // placeholder - bare identifiers start with alphanumeric
                }
            }
            SurfaceExpression::VarRef { escaped, .. } => {
                if *escaped {
                    Some('$')
                } else {
                    Some('a') // placeholder
                }
            }
            SurfaceExpression::DotAccess { .. } => Some('a'), // starts with whatever the base expr is
            SurfaceExpression::Pipe { .. } => Some('a'),      // starts with lhs
            SurfaceExpression::Sequential(_) => Some('('),    // starts with (seq
            SurfaceExpression::Dict(_)
            | SurfaceExpression::Call { .. }
            | SurfaceExpression::Fn { .. } => Some('['),
            SurfaceExpression::TypeAssert { .. } => Some('['),
            SurfaceExpression::Quote(_)
            | SurfaceExpression::Unquote(_)
            | SurfaceExpression::UnquoteSplice(_)
            | SurfaceExpression::Match { .. }
            | SurfaceExpression::PatternDecl { .. }
            | SurfaceExpression::LetDecl { .. }
            | SurfaceExpression::CaseArm { .. } => Some('['),
            SurfaceExpression::Annotated { name, .. } => name.chars().next(),
            SurfaceExpression::Placeholder
            | SurfaceExpression::Rest(_)
            | SurfaceExpression::Decl(_) => Some('.'),
            SurfaceExpression::Error(_) => None,
        }
    }
}

/// Check if a character is a bare-word character (can appear in unquoted identifiers).
/// Used by the `--nospaces` mode to determine when spaces are required.
fn is_bare_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '?' | '!' | '/' | '%' | '~')
}

/// Check if a character is a stop character for variable names in interpolated strings.
/// Mirrors the stop-char set in `src/lexer.rs` `lex_interpolated_string` VarRef branch
/// and the `tmpl-is-stop` helper in `stdlib/prelude.llt`.
fn is_tmpl_stop_char(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '[' | ']' | ':' | '#' | ';' | '"' | '@' | '.' | ',' | '!' | '?'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert_eq!(format_source("").unwrap(), "\n");
    }

    #[test]
    fn test_empty_brackets() {
        assert_eq!(format_source("[]").unwrap(), "[]\n");
    }

    #[test]
    fn test_simple_dict() {
        assert_eq!(format_source("[x: 1]").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_semicolons_removed() {
        assert_eq!(format_source("[x: 1; y: 2]").unwrap(), "[x: 1 y: 2]\n");
    }

    #[test]
    fn test_key_value_spacing() {
        assert_eq!(format_source("[x:1]").unwrap(), "[x: 1]\n");
        assert_eq!(format_source("[x :  1]").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_single_line_four_entries() {
        let input = "[a: 1 b: 2 c: 3 d: 4]";
        assert_eq!(format_source(input).unwrap(), "[a: 1 b: 2 c: 3 d: 4]\n");
    }

    #[test]
    fn test_multi_line_five_entries() {
        let input = "[a: 1 b: 2 c: 3 d: 4 e: 5]";
        let expected = "[\n  a: 1\n  b: 2\n  c: 3\n  d: 4\n  e: 5\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_multi_line_width_exceeded() {
        let long_value = "a".repeat(76);
        let input = format!("[x: {long_value}]");
        // bare identifier — no `$` prefix
        let expected = format!("[\n  x: {long_value}\n]\n");
        assert_eq!(format_source(&input).unwrap(), expected);
    }

    #[test]
    fn test_indentation_nested() {
        let input = "[outer: [inner: [deep: 1]]]";
        assert_eq!(
            format_source(input).unwrap(),
            "[outer: [inner: [deep: 1]]]\n"
        );
    }

    #[test]
    fn test_indentation_nested_multi_line() {
        let input = "[a: 1 b: 2 c: [x: 1 y: 2] d: 3 e: 4]";
        let expected = "[\n  a: 1\n  b: 2\n  c: [x: 1 y: 2]\n  d: 3\n  e: 4\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_trailing_comment() {
        let input = "[x: 1 # this is x\ny: 2]";
        let expected = "[\n  x: 1 # this is x\n  y: 2\n]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_leading_comment() {
        let input = "# header comment\n[x: 1]";
        assert_eq!(format_source(input).unwrap(), "# header comment\n[x: 1]\n");
    }

    #[test]
    fn test_section_comment() {
        let input = "[a: 1]\n\n# Section B\n[b: 2]";
        assert_eq!(
            format_source(input).unwrap(),
            "[a: 1]\n\n# Section B\n[b: 2]\n"
        );
    }

    #[test]
    fn test_blank_line_collapsing() {
        let input = "[x: 1]\n\n\n\n[y: 2]";
        // AST-based formatter normalizes formatting - simple dicts on one line
        assert_eq!(format_source(input).unwrap(), "[x: 1] [y: 2]\n");
    }

    #[test]
    fn test_doc_separator() {
        let input = "[x: 1]\n---\n[y: 2]";
        assert_eq!(format_source(input).unwrap(), "[x: 1]\n\n---\n\n[y: 2]\n");
    }

    #[test]
    fn test_doc_separator_first_document() {
        let input = "[x: 1]\n---\n[y: 2]";
        let expected = "[x: 1]\n\n---\n\n[y: 2]\n";
        assert_eq!(format_source(input).unwrap(), expected);
    }

    #[test]
    fn test_trailing_newline() {
        assert_eq!(format_source("[x: 1]").unwrap(), "[x: 1]\n");
        assert_eq!(format_source("[x: 1]\n\n\n").unwrap(), "[x: 1]\n");
    }

    #[test]
    fn test_access_chain() {
        assert_eq!(format_source("$a.b.c").unwrap(), "$a.b.c\n");
    }

    #[test]
    fn test_access_chain_in_dict() {
        assert_eq!(format_source("[x: $a.b.c]").unwrap(), "[x: $a.b.c]\n");
    }

    #[test]
    fn test_bracket_no_longer_access() {
        // Bracket access syntax has been removed. `$a` and `[0]` are now separate expressions.
        // The formatter handles each as a standalone item in the document.
        // $a alone formats as a reference.
        assert_eq!(format_source("$a").unwrap(), "$a\n");
        // [0] alone formats as a dict with one auto-indexed entry.
        assert_eq!(format_source("[0]").unwrap(), "[0]\n");
    }

    #[test]
    fn test_immediate_at_spacing() {
        // ImmediateAt in type-assert context: no space before @ — stays without space
        assert_eq!(format_source("[@Int 42]").unwrap(), "[@Int 42]\n");
        // Annotation in param context — formatter now emits [let ...] form
        assert_eq!(
            format_source("[fn [let x@Int] $x]").unwrap(),
            "[fn [let x@Int] $x]\n"
        );
    }

    #[test]
    fn test_annotation_no_spaces() {
        // Annotations in type-assert context
        assert_eq!(format_source("[@Number 42]").unwrap(), "[@Number 42]\n");
        assert_eq!(format_source("[@ Number 42]").unwrap(), "[@Number 42]\n");
    }

    #[test]
    fn test_quoted_string_preserved() {
        let input = r#"[msg: "hello\nworld"]"#;
        assert_eq!(format_source(input).unwrap(), "[msg: \"hello\\nworld\"]\n");
    }

    #[test]
    fn test_idempotency() {
        let input = "[a: 1 b: 2 c: 3 d: 4]";
        let formatted = format_source(input).unwrap();
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(formatted, reformatted);
    }

    #[test]
    fn test_idempotency_multi_line() {
        let input = "[\n  a: 1\n  b: 2\n  c: 3\n  d: 4\n  e: 5\n]\n";
        let reformatted = format_source(input).unwrap();
        assert_eq!(input, reformatted);
    }

    #[test]
    fn test_variadic_rest() {
        assert_eq!(format_source("[... x]").unwrap(), "[...x]\n");
        assert_eq!(format_source("[...rest]").unwrap(), "[...rest]\n");
    }

    #[test]
    fn test_positional_entries() {
        assert_eq!(format_source("[1 2 3]").unwrap(), "[1 2 3]\n");
    }

    #[test]
    fn test_deeply_nested() {
        let input = "[a: [b: [c: [d: [e: 1]]]]]";
        assert_eq!(
            format_source(input).unwrap(),
            "[a: [b: [c: [d: [e: 1]]]]]\n"
        );
    }

    #[test]
    fn test_fn_params_always_single_line() {
        // Formatter now emits [let ...] form for fn params
        let input = "[fn [let param1 param2 param3 param4 param5] [x: 1]]";
        let formatted = format_source(input).unwrap();
        assert!(formatted.contains("[let param1 param2 param3 param4 param5]"));
    }

    #[test]
    fn test_boolean_literals() {
        assert_eq!(
            format_source("[x: true y: false]").unwrap(),
            "[x: true y: false]\n"
        );
    }

    #[test]
    fn test_negative_numbers() {
        assert_eq!(format_source("[-1 -2.5]").unwrap(), "[-1 -2.5]\n");
    }

    #[test]
    fn test_var_refs() {
        assert_eq!(format_source("[$x $y]").unwrap(), "[$x $y]\n");
        assert_eq!(format_source("[$$]").unwrap(), "[$$]\n");
    }

    #[test]
    fn test_complex_real_world() {
        let input = r#"
[
  name: "test"
  version: 1
  deps: [lodash react vue]
  config: [
    timeout: 30
    retries: 3
  ]
]
"#;
        let formatted = format_source(input).unwrap();
        assert!(formatted.contains("[\n  name: \"test\""));
        assert!(formatted.contains("deps:") && formatted.contains("lodash"));
        assert!(formatted.contains("config:"));
    }

    #[test]
    fn test_error_invalid_input() {
        let result = format_source("[unterminated string \"hello");
        assert!(result.is_err());
    }

    #[test]
    fn test_call_expression_single_line() {
        // `$func` is an EscapedRef → keeps `$`; `arg1/arg2/arg3` are bare identifiers → no `$`
        let input = "[call $func arg1 arg2 arg3]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[call $func arg1 arg2 arg3]\n");
    }

    #[test]
    fn test_call_expression_many_args_single_line() {
        let input = "[call $if $cond $then $else]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[call $if $cond $then $else]\n");
    }

    #[test]
    fn test_implied_call_roundtrips() {
        // Implied call with bare identifier head and bare identifier args
        assert_eq!(format_source("[f x y]").unwrap(), "[f x y]\n");
        // Zero-arg implied call
        assert_eq!(format_source("[clock]").unwrap(), "[clock]\n");
        // Single-arg implied call
        assert_eq!(format_source("[negate n]").unwrap(), "[negate n]\n");
        // EscapedRef args keep their `$`
        assert_eq!(format_source("[f $x $y]").unwrap(), "[f $x $y]\n");
    }

    #[test]
    fn test_fn_definition_single_line() {
        // Formatter now emits [let ...] form for fn params
        let input = "[fn [let x y] [call $add $x $y]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[fn [let x y] [call $add $x $y]]\n");
    }

    #[test]
    fn test_comment_forces_multi_line() {
        let input = "[a: 1 # comment\nb: 2]";
        let formatted = format_source(input).unwrap();
        assert!(formatted.starts_with("[\n"));
        assert!(formatted.contains("  a: 1 # comment"));
        assert!(formatted.contains("  b: 2"));
    }

    #[test]
    fn test_top_level_spacing_preserved() {
        assert_eq!(format_source("$a $b").unwrap(), "$a $b\n");
    }

    #[test]
    fn test_top_level_bracket_not_merged() {
        let input = "$a [x: 1]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "$a [x: 1]\n");
    }

    #[test]
    fn test_top_level_access_chain() {
        assert_eq!(format_source("$a.b.c").unwrap(), "$a.b.c\n");
    }

    #[test]
    fn test_top_level_annotation() {
        // Type-assert at top level
        assert_eq!(format_source("[@Number 42]").unwrap(), "[@Number 42]\n");
    }

    #[test]
    fn test_annotated_key() {
        // Annotated param in function — formatter now emits [let ...] form
        assert_eq!(
            format_source("[fn [let x@Int] $x]").unwrap(),
            "[fn [let x@Int] $x]\n"
        );
    }

    #[test]
    fn test_formatter_error_unterminated_string() {
        let result = format_source("[\"unterminated");
        assert!(
            result.is_err(),
            "unterminated string should produce a lex error"
        );
    }

    #[test]
    fn test_formatter_error_bare_dollar() {
        let result = format_source("$");
        assert!(
            result.is_err(),
            "bare $ without identifier should produce a lex error"
        );
    }

    #[test]
    fn test_formatter_error_invalid_escape() {
        let result = format_source(r#""\q""#);
        assert!(
            result.is_err(),
            "invalid escape sequence \\q should produce a lex error"
        );
    }

    // --- Interpolated string round-trip tests ---

    /// i"Hello $name" should round-trip back to i"Hello $name" (not [str "Hello " name]).
    #[test]
    fn test_interpolated_string_roundtrip_simple() {
        let input = r#"i"Hello $name""#;
        let formatted = format_source(input).unwrap();
        assert_eq!(
            formatted.trim(),
            r#"i"Hello $name""#,
            "i\"...\" with single variable should round-trip as i\"...\""
        );
    }

    /// i"$a and $b" (multiple variables) should round-trip.
    #[test]
    fn test_interpolated_string_roundtrip_multi_var() {
        let input = r#"i"$a and $b""#;
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), r#"i"$a and $b""#);
    }

    /// Plain i"text" (no variables) should round-trip.
    #[test]
    fn test_interpolated_string_roundtrip_plain() {
        let input = r#"i"plain text""#;
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), r#"i"plain text""#);
    }

    /// Formatter idempotency: formatting i"..." twice gives the same result.
    #[test]
    fn test_interpolated_string_format_idempotency() {
        let input = r#"i"Hello $name, you are $age years old""#;
        let once = format_source(input).unwrap();
        let twice = format_source(&once).unwrap();
        assert_eq!(once, twice, "i\"...\" formatting must be idempotent");
    }

    /// Literal $ chars (desugared from $$) are escaped back to $$ in i"..." output.
    #[test]
    fn test_interpolated_string_roundtrip_dollar_escape() {
        let input = r#"i"cost: $$10""#;
        let formatted = format_source(input).unwrap();
        assert_eq!(
            formatted.trim(),
            r#"i"cost: $$10""#,
            "literal $ inside i\"...\" should be emitted as $$"
        );
    }

    // --- _ implicit lambda round-trip tests ---

    /// Format `[map _.age users]`, re-parse, check AST equality.
    ///
    /// The formatter must preserve `_` verbatim. After formatting and re-parsing
    /// the AST should be structurally identical to parsing the original source.
    #[test]
    fn test_underscore_roundtrip_call_dot_access() {
        let input = "[map _.age users]";
        let formatted = format_source(input).unwrap();
        // Formatter should output the same structure (single-line, within width limit)
        assert_eq!(formatted.trim(), "[map _.age users]");

        // Parse original and re-parsed AST should be equal
        let ast_original = crate::parser::parse_surface_expression(input).unwrap();
        let ast_reparsed = crate::parser::parse_surface_expression(formatted.trim()).unwrap();
        assert_eq!(
            ast_original.expr, ast_reparsed.expr,
            "AST after format-reparse should equal original AST"
        );
    }

    /// Format `[filter _.active users]` and verify round-trip.
    #[test]
    fn test_underscore_roundtrip_call_field_filter() {
        let input = "[filter _.active users]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "[filter _.active users]");

        let ast_original = crate::parser::parse_surface_expression(input).unwrap();
        let ast_reparsed = crate::parser::parse_surface_expression(formatted.trim()).unwrap();
        assert_eq!(ast_original.expr, ast_reparsed.expr);
    }

    /// Format `[+ _ 1]` (bare _ in arg position) and verify round-trip.
    #[test]
    fn test_underscore_roundtrip_bare_arg() {
        let input = "[+ _ 1]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "[+ _ 1]");

        let ast_original = crate::parser::parse_surface_expression(input).unwrap();
        let ast_reparsed = crate::parser::parse_surface_expression(formatted.trim()).unwrap();
        assert_eq!(ast_original.expr, ast_reparsed.expr);
    }

    /// Format `_.name.first` (chained dot access on _) and verify round-trip.
    #[test]
    fn test_underscore_roundtrip_chained_dot_access() {
        let input = "_.name.first";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "_.name.first");

        let ast_original = crate::parser::parse_surface_expression(input).unwrap();
        let ast_reparsed = crate::parser::parse_surface_expression(formatted.trim()).unwrap();
        assert_eq!(ast_original.expr, ast_reparsed.expr);
    }

    /// Formatter idempotency with _ — format twice produces same output.
    #[test]
    fn test_underscore_format_idempotency() {
        let input = "[map _.age users]";
        let formatted_once = format_source(input).unwrap();
        let formatted_twice = format_source(&formatted_once).unwrap();
        assert_eq!(
            formatted_once, formatted_twice,
            "_ formatting should be idempotent"
        );
    }

    /// Test that CRLF line endings are handled correctly during format round-trip.
    /// The formatter should parse CRLF input and produce normalized output.
    #[test]
    fn test_crlf_roundtrip() {
        // Input with CRLF line endings
        let input = "[x: 1\r\ny: 2\r\n]";
        let formatted = format_source(input).unwrap();
        // Formatter normalizes CRLF to LF — output must not contain CR bytes
        assert!(
            !formatted.contains("\r\n"),
            "formatter must normalize CRLF to LF"
        );
        // Formatter should successfully parse and produce valid output
        assert!(formatted.contains("x: 1"));
        assert!(formatted.contains("y: 2"));
        // Re-parsing the formatted output should succeed
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(formatted, reformatted, "CRLF formatting must be idempotent");
    }

    // --- Named section header round-trip tests ---

    #[test]
    fn test_named_section_header_roundtrips() {
        // Basic named section: separator + name should be emitted
        let input = "[x: 1]\n--- %defaults\n[y: 2]";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("--- %defaults"),
            "formatted output must include section header '--- %defaults', got: {formatted:?}"
        );
        // Idempotency: formatting a second time should produce the same result
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "named section formatting must be idempotent"
        );
    }

    #[test]
    fn test_named_section_with_output_type_roundtrips() {
        let input = "[x: 1]\n--- %cfg@Dict\n[y: 2]";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("--- %cfg@Dict"),
            "formatted output must include '--- %cfg@Dict' (no space before @), got: {formatted:?}"
        );
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "named section with @Type must be idempotent"
        );
    }

    #[test]
    fn test_standalone_output_type_has_space() {
        let input = "[x: 1]\n--- @Config\n[y: 2]";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("--- @Config"),
            "formatted output must include '--- @Config' (space before @), got: {formatted:?}"
        );
        // Verify idempotency
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "standalone @Type section must be idempotent"
        );
    }

    #[test]
    fn test_named_section_with_expects_roundtrips() {
        let input = "[x: 1]\n--- %out expects: @Dict\n[y: 2]";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("--- %out"),
            "formatted output must include section name, got: {formatted:?}"
        );
        assert!(
            formatted.contains("expects: @Dict"),
            "formatted output must include expects pragma, got: {formatted:?}"
        );
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "named section with expects: must be idempotent"
        );
    }

    #[test]
    fn test_unnamed_section_separator_unchanged() {
        // Unnamed documents should still emit bare ---
        let input = "[x: 1]\n---\n[y: 2]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[x: 1]\n\n---\n\n[y: 2]\n");
    }

    #[test]
    fn test_format_quote() {
        let input = "[quote [+ 1 2]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[quote [+ 1 2]]\n");
    }

    #[test]
    fn test_format_unquote() {
        let input = "[quote [+ [unquote x] 2]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[quote [+ [unquote x] 2]]\n");
    }

    #[test]
    fn test_format_unquote_splice() {
        let input = "[quote [call f [unquote-splice args]]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[quote [call f [unquote-splice args]]]\n");
    }

    #[test]
    fn test_format_nested_quote() {
        let input = "[quote [quote [unquote x]]]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "[quote [quote [unquote x]]]\n");
    }

    #[test]
    fn test_format_float_whole_number() {
        // f64::to_string() for 1.0 yields "1" with no '.' or 'e',
        // so the formatter must append ".0" to preserve the float literal.
        let result = format_source("[x: 1.0]").unwrap();
        assert_eq!(result, "[x: 1.0]\n");
    }

    #[test]
    fn test_format_float_with_decimal() {
        let result = format_source("[x: 3.14]").unwrap();
        assert_eq!(result, "[x: 3.14]\n");
    }

    // --- Match / Pipe expression tests ---

    #[test]
    fn test_format_match_single_arm() {
        // Match syntax requires a colon between pattern and body.
        // Dict patterns are open by default (rest: true), so [ok: v] formats as [ok: v  ...].
        let input = "[match $x [ok: v]: $v]";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "[match $x [ok: v  ...]: $v]");
    }

    #[test]
    fn test_format_match_two_arms() {
        // Match syntax requires a colon between each pattern and its body.
        // Dict patterns are open by default (rest: true), so [ok: body] formats as [ok: body  ...].
        let input = "[match $r [ok: body]: $body [err: msg]: $msg]";
        let formatted = format_source(input).unwrap();
        assert_eq!(
            formatted.trim(),
            "[match $r [ok: body  ...]: $body [err: msg  ...]: $msg]"
        );
    }

    #[test]
    fn test_format_match_idempotent() {
        let input = "[match $r [ok: body]: $body [err: msg]: $msg]";
        let once = format_source(input).unwrap();
        let twice = format_source(&once).unwrap();
        assert_eq!(
            once, twice,
            "match expression formatting must be idempotent"
        );
    }

    #[test]
    fn test_format_pipe_simple() {
        // $x | $f — the pipe operator
        let input = "$x | $f";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "$x | $f");
    }

    #[test]
    fn test_format_pipe_chained() {
        let input = "$x | $f | $g";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted.trim(), "$x | $f | $g");
    }

    #[test]
    fn test_format_pipe_idempotent() {
        let input = "$x | $f";
        let once = format_source(input).unwrap();
        let twice = format_source(&once).unwrap();
        assert_eq!(once, twice, "pipe expression formatting must be idempotent");
    }

    #[test]
    fn test_uses_header_roundtrips() {
        // --- uses: ["core"] header must survive a format/parse/format round-trip.
        let input = "--- uses: [\"core\"]\n[x: 1]\n";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("uses: [\"core\"]"),
            "formatted output must include 'uses: [\"core\"]', got: {formatted:?}"
        );
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "uses: header must be idempotent under repeated formatting"
        );
    }

    #[test]
    fn test_uses_header_multi_module_roundtrips() {
        // Multiple modules in a uses: header.
        let input = "--- uses: [\"core\" \"datetime\"]\n[x: 1]\n";
        let formatted = format_source(input).unwrap();
        assert!(
            formatted.contains("uses: [\"core\" \"datetime\"]"),
            "formatted output must include both modules, got: {formatted:?}"
        );
        let reformatted = format_source(&formatted).unwrap();
        assert_eq!(
            formatted, reformatted,
            "multi-module uses: header must be idempotent under repeated formatting"
        );
    }
}
