//! Macro expansion pass for `[defmacro ...]` forms.
//!
//! Runs between parse and desugar: `parse -> expand_macros -> desugar -> typecheck -> eval`
//!
//! The expansion loop:
//! 1. Walk the AST top-down
//! 2. Register `DefMacro` nodes by evaluating their transformer in a fresh context
//! 3. Expand `Call` nodes if the function name matches a registered macro:
//!    - Quote arguments via `ast_to_dict_expr`
//!    - Call the macro transformer with the quoted args
//!    - Convert the result back to AST via `dict_to_ast`
//!    - Replace the Call node with the expansion
//!    - Re-expand the result (fixpoint)
//! 4. Track in-progress expansions to detect infinite recursion

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Document, Entry, Expr, File, NamedArg, Span, Spanned};
use crate::ast_dict::{ast_to_dict_expr, dict_to_ast, AstToDictOpts};
use crate::builtins;
use crate::error::{EvalError, EvalResult};
use crate::eval::{self, EvalContext};
use crate::value::{Environment, Key, Thunk, Value};

/// Macro expansion context — tracks registered macros and prevents infinite expansion.
#[derive(Debug, Clone)]
pub struct MacroEnv {
    /// Map from macro name to the transformer function (as a Value).
    /// The transformer is a function that takes AST dicts and returns an AST dict.
    macros: HashMap<String, Rc<Thunk>>,
    /// Expansion depth counter — prevents deeply nested expansions.
    depth: usize,
    /// Total node count expanded — prevents runaway macro generation.
    node_count: usize,
    /// In-progress call sites: (file_id, byte_offset) or synthetic ID for generated nodes.
    /// Used for blackhole detection.
    in_progress: HashSet<CallSiteId>,
}

/// Unique identifier for a macro call site.
/// Source nodes use (file_id, byte_offset); generated nodes use a synthetic counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CallSiteId {
    Source { file_id: usize, offset: usize },
    Synthetic(u64),
}

/// Synthetic node ID counter (for macro-generated code with no source span).
static mut SYNTHETIC_COUNTER: u64 = 0;

fn next_synthetic_id() -> u64 {
    unsafe {
        SYNTHETIC_COUNTER += 1;
        SYNTHETIC_COUNTER
    }
}

impl MacroEnv {
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
            depth: 0,
            node_count: 0,
            in_progress: HashSet::new(),
        }
    }

    /// Register a macro transformer.
    /// Returns an error if the name collides with a registered Rust builtin.
    fn register_macro(
        &mut self,
        name: String,
        transformer: Rc<Thunk>,
        span: Span,
    ) -> EvalResult<()> {
        // Check if the name collides with a registered builtin
        let builtin_names: HashSet<String> = builtins::standard_builtins()
            .iter()
            .map(|def| def.name.to_string())
            .collect();

        if builtin_names.contains(&name) {
            return Err(EvalError::user_error(
                format!(
                    "macro name '{}' collides with registered builtin — macros cannot shadow builtins",
                    name
                ),
                span,
            )
            .into());
        }
        self.macros.insert(name, transformer);
        Ok(())
    }

    /// Check if a name is a registered macro.
    fn is_macro(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Get the transformer for a macro.
    fn get_transformer(&self, name: &str) -> Option<&Rc<Thunk>> {
        self.macros.get(name)
    }

    /// Enter a macro expansion (depth check).
    fn enter_expansion(&mut self, call_site: CallSiteId, span: Span) -> EvalResult<()> {
        const MAX_DEPTH: usize = 100;
        const MAX_NODE_COUNT: usize = 100_000;

        if self.depth >= MAX_DEPTH {
            return Err(EvalError::user_error(
                format!(
                    "macro expansion depth limit exceeded ({} levels)",
                    MAX_DEPTH
                ),
                span,
            )
            .into());
        }

        if self.node_count >= MAX_NODE_COUNT {
            return Err(EvalError::user_error(
                format!(
                    "macro expansion node count limit exceeded ({} nodes)",
                    MAX_NODE_COUNT
                ),
                span,
            )
            .into());
        }

        if self.in_progress.contains(&call_site) {
            return Err(EvalError::user_error(
                "recursive macro expansion detected (macro expanding itself)".to_string(),
                span,
            )
            .into());
        }

        self.depth += 1;
        self.node_count += 1;
        self.in_progress.insert(call_site);
        Ok(())
    }

    /// Leave a macro expansion.
    fn leave_expansion(&mut self, call_site: CallSiteId) {
        self.depth -= 1;
        self.in_progress.remove(&call_site);
    }
}

/// Expand all macros in a File AST.
///
/// This is the top-level entry point called from the pipeline.
/// Takes a no_fs flag to match the pipeline configuration.
pub fn expand_macros(file: Spanned<File>, no_fs: bool) -> EvalResult<Spanned<File>> {
    let mut env_macro = MacroEnv::new();

    // Create a minimal eval context for macro expansion
    // Use current directory as base_dir
    let base_dir_path = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| {
            EvalError::internal(
                format!("cannot open base directory for macro expansion: {e}"),
                file.span,
            )
        })?;

    let stdlib_env = builtins::create_stdlib_env().map_err(|e| {
        EvalError::internal(
            format!("cannot create stdlib env for macro expansion: {e}"),
            file.span,
        )
    })?;

    let ctx = Rc::new(EvalContext::new(base_dir, Rc::clone(&stdlib_env), no_fs));

    // Process each document in the file
    let expanded_documents = file
        .node
        .documents
        .into_iter()
        .map(|doc| {
            let expanded_doc = expand_document(doc.node, &mut env_macro, &ctx, &stdlib_env)?;
            Ok(Spanned::new(expanded_doc, doc.span))
        })
        .collect::<EvalResult<Vec<_>>>()?;

    Ok(Spanned::new(
        File {
            documents: expanded_documents,
        },
        file.span,
    ))
}

/// Expand macros in a Document.
fn expand_document(
    doc: Document,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Document> {
    // Expand each expression in the document, filtering out DefMacro nodes
    let mut expanded_exprs = Vec::new();

    for expr in doc.expressions {
        let expanded = expand_expr(expr.as_ref().clone(), env, ctx, stdlib_env)?;
        // Filter out DefMacro nodes (they've been registered and should not appear post-expansion)
        if !matches!(expanded.node, Expr::DefMacro { .. }) {
            expanded_exprs.push(Rc::new(expanded));
        }
    }

    Ok(Document {
        expressions: expanded_exprs,
        name: doc.name,
        output_type: doc.output_type,
        expects: doc.expects,
    })
}

/// Expand macros in an expression (fixpoint loop).
fn expand_expr(
    expr: Spanned<Expr>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Spanned<Expr>> {
    match &expr.node {
        Expr::DefMacro { name, transformer } => {
            // Evaluate the transformer in the stdlib environment
            let transformer_value = eval::eval(
                Rc::new(transformer.as_ref().clone()),
                Rc::clone(stdlib_env),
                ctx,
                0,
            )?;

            // Register the macro
            env.register_macro(name.clone(), transformer_value, expr.span)?;

            // Return the DefMacro node unchanged (will be filtered out by expand_document)
            Ok(expr)
        }

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // Check if this is a macro call (func is a VarRef to a registered macro)
            let macro_name = if let Expr::VarRef { name, .. } = &func.node {
                if env.is_macro(name) {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(macro_name) = macro_name {
                // This is a macro call — expand it
                expand_macro_call(
                    &macro_name,
                    args,
                    named_args,
                    expr.span,
                    env,
                    ctx,
                    stdlib_env,
                )
            } else {
                // Not a macro call — recursively expand children
                let expanded_func = expand_expr(func.as_ref().clone(), env, ctx, stdlib_env)?;
                let expanded_args = args
                    .iter()
                    .map(|arg| {
                        let expanded = expand_expr(arg.as_ref().clone(), env, ctx, stdlib_env)?;
                        Ok(Rc::new(expanded))
                    })
                    .collect::<EvalResult<Vec<_>>>()?;
                let expanded_named_args = named_args
                    .iter()
                    .map(|named_arg| {
                        let expanded_value = expand_expr(
                            named_arg.node.value.as_ref().clone(),
                            env,
                            ctx,
                            stdlib_env,
                        )?;
                        Ok(Spanned::new(
                            NamedArg {
                                name: named_arg.node.name.clone(),
                                value: Rc::new(expanded_value),
                            },
                            named_arg.span,
                        ))
                    })
                    .collect::<EvalResult<Vec<_>>>()?;

                Ok(Spanned::new(
                    Expr::Call {
                        func: Box::new(expanded_func),
                        args: expanded_args,
                        named_args: expanded_named_args,
                        implied: *implied,
                    },
                    expr.span,
                ))
            }
        }

        // Recursively expand other expression types
        Expr::DotAccess {
            expr: target,
            field,
        } => {
            let expanded_target = expand_expr(target.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::DotAccess {
                    expr: Box::new(expanded_target),
                    field: field.clone(),
                },
                expr.span,
            ))
        }

        Expr::Pipe { lhs, rhs } => {
            let expanded_lhs = expand_expr(lhs.as_ref().clone(), env, ctx, stdlib_env)?;
            let expanded_rhs = expand_expr(rhs.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Pipe {
                    lhs: Box::new(expanded_lhs),
                    rhs: Box::new(expanded_rhs),
                },
                expr.span,
            ))
        }

        Expr::Dict(entries) => {
            let expanded_entries = entries
                .iter()
                .map(|entry| {
                    let expanded_value =
                        expand_expr(entry.node.value.as_ref().clone(), env, ctx, stdlib_env)?;
                    let expanded_key = if let Some(key) = &entry.node.key {
                        Some(expand_expr(key.clone(), env, ctx, stdlib_env)?)
                    } else {
                        None
                    };
                    Ok(Spanned::new(
                        Entry {
                            key: expanded_key,
                            value: Rc::new(expanded_value),
                        },
                        entry.span,
                    ))
                })
                .collect::<EvalResult<Vec<_>>>()?;

            Ok(Spanned::new(Expr::Dict(expanded_entries), expr.span))
        }

        Expr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            let expanded_body = expand_expr(body.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: Rc::new(expanded_body),
                    desugared: *desugared,
                },
                expr.span,
            ))
        }

        Expr::TypeAlias(type_expr) => {
            let expanded_type_expr = expand_expr(type_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::TypeAlias(Box::new(expanded_type_expr)),
                expr.span,
            ))
        }

        Expr::TypeAssert {
            annotation,
            expr: asserted_expr,
            resolved_type,
        } => {
            let expanded_expr = expand_expr(asserted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::TypeAssert {
                    annotation: annotation.clone(),
                    expr: Box::new(expanded_expr),
                    resolved_type: resolved_type.clone(),
                },
                expr.span,
            ))
        }

        Expr::Quote(quoted_expr) => {
            let expanded_quoted = expand_expr(quoted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Quote(Box::new(expanded_quoted)),
                expr.span,
            ))
        }

        Expr::Unquote(unquoted_expr) => {
            let expanded_unquoted =
                expand_expr(unquoted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Unquote(Box::new(expanded_unquoted)),
                expr.span,
            ))
        }

        Expr::UnquoteSplice(spliced_expr) => {
            let expanded_spliced =
                expand_expr(spliced_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::UnquoteSplice(Box::new(expanded_spliced)),
                expr.span,
            ))
        }

        // Leaf nodes — no expansion needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Annotated { .. }
        | Expr::Rest(_)
        | Expr::Error(_) => Ok(expr),
    }
}

/// Expand a macro call.
fn expand_macro_call(
    macro_name: &str,
    args: &[Rc<Spanned<Expr>>],
    _named_args: &[Spanned<NamedArg>],
    call_span: Span,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Spanned<Expr>> {
    // Determine call site ID for blackhole detection
    let call_site_id = if call_span.start.offset == 0 && call_span.start.line == 1 {
        // Synthetic span (origin) — generated code
        CallSiteId::Synthetic(next_synthetic_id())
    } else {
        // Source span — use file_id=0 (single-file assumption for now)
        CallSiteId::Source {
            file_id: 0,
            offset: call_span.start.offset,
        }
    };

    // Enter expansion (depth check + blackhole detection)
    env.enter_expansion(call_site_id, call_span)?;

    // TODO: Actually call the transformer with quoted args and expand the result.
    // For now, just return a placeholder that shows macro expansion is working.
    // This is Phase 1 - basic infrastructure only.

    // Build a simple identity expansion: just return the first argument as-is
    let expanded_ast = if !args.is_empty() {
        args[0].as_ref().clone()
    } else {
        // No args - return empty dict
        Spanned::new(Expr::Dict(vec![]), call_span)
    };

    // Leave expansion
    env.leave_expansion(call_site_id);

    // Re-expand the result (fixpoint)
    expand_expr(expanded_ast, env, ctx, stdlib_env)
}
